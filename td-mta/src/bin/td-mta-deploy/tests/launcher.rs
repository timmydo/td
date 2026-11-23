#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::*;
use td_fs::TempDir;

#[test]
fn accepts_only_exact_release_and_origin_fields() {
    let rev = "0123456789abcdef0123456789abcdef01234567";
    assert_eq!(advertised(&format!("{rev}\trefs/heads/main")).unwrap(), rev);
    assert!(advertised(&format!("{rev} refs/heads/main extra")).is_err());
    assert_eq!(live_path(&format!("{RELEASES}{rev}/td-mta")).unwrap(), rev);
    assert!(live_path(&format!("{RELEASES}{rev}/another")).is_err());
    assert_eq!(
        words(&["hello world", "a'b"]).unwrap(),
        "'hello world' 'a'\\''b'"
    );
    assert!(parse(&["--skip".into()]).is_err());
    assert!(parse(&["--s".into()]).is_err());
    assert!(parse(&["--skip-tests".into()]).unwrap().skip);
    assert!(parse(&["--window-seconds".into(), "0".into()]).is_err());
    assert!(parse(&["--postgres-socket".into()]).unwrap().socket);
    assert!(!parse(&[]).unwrap().socket);
    assert!(parse(&["--postgres-socket".into(), "--skip-tests".into()]).is_err());
    let defaults = parse(&[]).unwrap();
    assert_eq!((defaults.account, defaults.config), (None, None));
    let args = [
        "--jmap-account".into(),
        "alice".into(),
        "--config".into(),
        "/home/alice/target.json".into(),
    ];
    let chosen = parse(&args).unwrap();
    assert_eq!(chosen.account, Some("alice"));
    assert_eq!(chosen.config, Some("/home/alice/target.json"));
    assert!(parse(&["--config".into()]).is_err());
}
fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@localhost",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}
fn commit(root: &Path, subject: &str) -> String {
    git(root, &["commit", "--allow-empty", "-qm", subject]);
    git(root, &["rev-parse", "HEAD"])
}
#[test]
fn selected_release_requires_passing_first_parent_and_live_ancestry_even_with_override() {
    let tmp = TempDir::new("mta-deploy-select").unwrap();
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    let before = commit(root, "before");
    let live = commit(root, "live");
    let passed = commit(root, "passed");
    git(root, &["checkout", "-qb", "side", &before]);
    let side = commit(root, "side");
    git(root, &["checkout", "-q", "main"]);
    git(root, &["merge", "-q", "--no-ff", "-m", "merge", "side"]);
    let tip = git(root, &["rev-parse", "HEAD"]);
    let verdicts = root.join("verdicts");
    fs::create_dir(&verdicts).unwrap();
    let local = Local {
        host: Host::default(),
        root: root.into(),
        destination: "alice@192.0.2.10".into(),
        key: PathBuf::new(),
    };
    fs::write(verdicts.join(&side), "pass 0 1 2\n").unwrap();
    fs::write(verdicts.join(&tip), "pass 0 incomplete\n").unwrap();
    assert_eq!(
        local.select(&tip, &live, false, &verdicts).unwrap(),
        (live.clone(), None)
    );
    fs::write(verdicts.join(&passed), "pass 0  123  456\n").unwrap();
    assert_eq!(
        local.select(&tip, &live, false, &verdicts).unwrap(),
        (passed, Some("pass 0 123 456".into()))
    );
    fs::write(verdicts.join(&tip), "pass 0 789 999").unwrap();
    assert_eq!(
        local.select(&tip, &live, false, &verdicts).unwrap(),
        (tip.clone(), Some("pass 0 789 999".into()))
    );
    fs::remove_dir_all(&verdicts).unwrap();
    assert_eq!(
        local.select(&tip, &live, true, &verdicts).unwrap(),
        (tip, None)
    );
    for skip in [false, true] {
        assert!(local.select(&side, &live, skip, &verdicts).is_err());
    }
}

#[test]
fn fetches_only_advertised_main_and_requires_an_already_submitted_controller() {
    let tmp = TempDir::new("mta-deploy-origin").unwrap();
    let root = tmp.path();
    let origin = root.join("origin");
    fs::create_dir(&origin).unwrap();
    git(&origin, &["init", "-q", "-b", "main"]);
    let live = commit(&origin, "live");
    git(
        root,
        &["clone", "-q", "--no-local", "--", "origin", "client"],
    );
    let client = root.join("client");
    let tip = commit(&origin, "tip");
    let local = Local {
        host: Host::default(),
        root: client.clone(),
        destination: "alice@192.0.2.10".into(),
        key: PathBuf::new(),
    };
    assert_eq!(local.origin_revisions().unwrap(), (tip.clone(), live));
    // A fetch by object ID leaves the local origin/main tracking ref unchanged.
    // The exact fetched object still qualifies through main ancestry.
    git(&client, &["checkout", "--detach", "-q", &tip]);
    assert_eq!(
        local.origin_revisions().unwrap(),
        (tip.clone(), tip.clone())
    );
    git(&client, &["checkout", "-qb", "controller"]);
    let submitted = commit(&client, "controller");
    assert!(local.origin_revisions().is_err());
    git(&client, &["push", "-q", "-u", "origin", "controller"]);
    assert_eq!(local.origin_revisions().unwrap(), (tip, submitted));
}

#[test]
fn the_binary_switch_takes_the_reviewed_release_and_nothing_else() {
    let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let both = args(&["--postgres-binaries", "--postgres-minor-reviewed", "17.6"]);
    let opts = parse(&both).unwrap();
    assert!(opts.binaries && !opts.socket);
    assert_eq!(opts.reviewed, Some("17.6"));
    for refused in [
        args(&["--postgres-binaries"]),
        args(&["--postgres-minor-reviewed", "17.6"]),
        args(&["--postgres-binaries", "--postgres-minor-reviewed", "16.4"]),
        args(&["--postgres-binaries", "--postgres-minor-reviewed", "17"]),
        args(&["--postgres-binaries", "--postgres-minor-reviewed"]),
        args(&[
            "--postgres-binaries",
            "--postgres-minor-reviewed",
            "17.6",
            "--skip-tests",
        ]),
        args(&[
            "--postgres-binaries",
            "--postgres-minor-reviewed",
            "17.6",
            "--postgres-socket",
        ]),
    ] {
        assert!(parse(&refused).is_err(), "{refused:?}");
    }
}

const PG: &str = "31f6m76fp7k1scqd064a892wz5d4aqcq-postgresql-x86-64-17.12";
const GLIBC: &str = "dgkqaph4a8q8hk5b5qg4prcp0q5q17x0-glibc-x86-64-2.41";

#[test]
fn build_run_reports_one_path_per_output() {
    let report = format!(
        "TD_RECIPE_RUN_WORK /w\nTD_RECIPE_RUN_OUT postgresql-x86-64 /s/tdstore/{PG}\n\
         TD_RECIPE_RUN_OUT glibc-x86-64 /s/tdstore/{GLIBC}\n"
    );
    let paths = closure::run_outputs(&report).unwrap();
    assert_eq!(
        paths,
        [
            PathBuf::from(format!("/s/tdstore/{PG}")),
            PathBuf::from(format!("/s/tdstore/{GLIBC}"))
        ]
    );
    for refused in [
        report.replace("glibc-x86-64 /", "zlib-x86-64 /"),
        report.replacen("TD_RECIPE_RUN_OUT glibc-x86-64", "x", 1),
        format!("{report}TD_RECIPE_RUN_OUT glibc-x86-64 /s/tdstore/{GLIBC}\n"),
        report.replace(&format!("/s/tdstore/{PG}"), &format!("s/tdstore/{PG}")),
        report.replace(PG, GLIBC),
    ] {
        assert!(closure::run_outputs(&refused).is_err(), "{refused}");
    }
}

#[test]
fn loaded_references_must_stay_inside_the_closure() {
    let interp =
        format!("/td/store/{GLIBC}/stage/td/store/glibc-2.41-x86_64/lib/ld-linux-x86-64.so.2");
    let report = format!(
        "ELF Header:\n  Type:                              DYN (Position-Independent Executable file)\n\
         Program Headers:\n      [Requesting program interpreter: {interp}]\n\
         Dynamic section at offset 0x1 contains 30 entries:\n\
         \x200x0000000000000001 (NEEDED)             Shared library: [libpq.so.5]\n\
         \x200x000000000000001d (RUNPATH)            Library runpath: [/td/store/{GLIBC}/stage/td/store/glibc-2.41-x86_64/lib:/td/store/{PG}/lib]\n"
    );
    let elf = closure::parse(&report).unwrap();
    assert!(elf.loadable);
    assert_eq!(elf.needed, ["libpq.so.5"]);
    let found: Vec<&str> = elf.references().copied().collect();
    let lib = format!("/td/store/{PG}/lib");
    assert_eq!(found.len(), 3);
    assert_eq!(found[0], interp);
    assert_eq!(found[2], lib);
    assert_eq!(elf.run_paths(), &found[1..]);
    let names = [PG, GLIBC];
    assert!(found.iter().all(|r| closure::in_closure(r, &names)));
    assert!(closure::parse(&report.replace("[libpq.so.5]", "[/usr/lib/libpq.so.5]")).is_err());
    let rpath = report.replace(
        "(RUNPATH)            Library runpath",
        "(RPATH) Library rpath",
    );
    let elf = closure::parse(&rpath).unwrap();
    assert_eq!(elf.references().copied().collect::<Vec<_>>(), found);
    assert_eq!(elf.run_paths(), &found[1..]);
    assert!(
        !closure::parse(&report.replace("DYN (", "REL ("))
            .unwrap()
            .loadable
    );
    for allowed in ["$ORIGIN", "$ORIGIN/lib", "${ORIGIN}/gconv"] {
        assert!(closure::in_closure(allowed, &names), "{allowed}");
    }
    for refused in [
        "/lib64/ld-linux-x86-64.so.2".to_owned(),
        "/opt/td-mta/postgresql/usr/lib/x86_64-linux-gnu".to_owned(),
        format!("/td/store/{PG}-other/lib"),
        format!("/td/store/{PG}/../evil/lib"),
        "/td/store/zzz-zlib-x86-64-1.4/lib".to_owned(),
        "$ORIGIN/../../evil".to_owned(),
        "$ORIGINAL".to_owned(),
        "lib".to_owned(),
        String::new(),
    ] {
        assert!(!closure::in_closure(&refused, &names), "{refused}");
    }
}

// A `readelf -h -l -d -W` report as the launcher reads one.
fn readelf(
    kind: &str,
    interp: Option<&str>,
    soname: Option<&str>,
    needed: &[&str],
    runpath: &[&str],
) -> String {
    let mut out = format!("ELF Header:\n  Type:                              {kind} (x)\n");
    if let Some(interp) = interp {
        out.push_str(&format!(
            "      [Requesting program interpreter: {interp}]\n"
        ));
    }
    for name in needed {
        out.push_str(&format!(
            " 0x1 (NEEDED)             Shared library: [{name}]\n"
        ));
    }
    if let Some(soname) = soname {
        out.push_str(&format!(
            " 0xe (SONAME)             Library soname: [{soname}]\n"
        ));
    }
    if !runpath.is_empty() {
        out.push_str(&format!(
            " 0x1d (RUNPATH)            Library runpath: [{}]\n",
            runpath.join(":")
        ));
    }
    out
}

#[test]
fn every_library_the_postgresql_output_loads_resolves_inside_the_closure() {
    let g = format!("/td/store/{GLIBC}/stage/td/store/glibc-2.41-x86_64/lib");
    let p = format!("/td/store/{PG}/lib");
    let loader = format!("{g}/ld-linux-x86-64.so.2");
    let both = [g.as_str(), p.as_str()];
    let files = |server_needs: &[&str], module_path: &[&str]| {
        vec![
            (
                format!("/td/store/{PG}/bin/postgres"),
                readelf("DYN", Some(&loader), None, server_needs, &both),
            ),
            (
                format!("{p}/libpq.so.5.17"),
                readelf(
                    "DYN",
                    None,
                    Some("libpq.so.5"),
                    &["libm.so.6", "libc.so.6"],
                    &both,
                ),
            ),
            (
                format!("{p}/pg_trgm.so"),
                readelf("DYN", None, None, &["libc.so.6"], module_path),
            ),
            (
                format!("{p}/utf8_and_big5.so"),
                readelf("DYN", None, None, &[], &[&g]),
            ),
            (
                loader.clone(),
                readelf("DYN", None, Some("ld-linux-x86-64.so.2"), &[], &[]),
            ),
            (
                format!("{g}/libc.so.6"),
                readelf(
                    "DYN",
                    Some(&loader),
                    Some("libc.so.6"),
                    &["ld-linux-x86-64.so.2"],
                    &[],
                ),
            ),
            (
                format!("{g}/libm.so.6"),
                readelf(
                    "DYN",
                    None,
                    Some("libm.so.6"),
                    &["libc.so.6", "ld-linux-x86-64.so.2"],
                    &[],
                ),
            ),
            (
                format!("{g}/libresolv.so.2"),
                readelf("DYN", None, Some("libresolv.so.2"), &["libc.so.6"], &[]),
            ),
            (
                format!("{g}/gconv/ISO-2022-KR.so"),
                readelf("DYN", None, None, &["libKSC.so", "libc.so.6"], &["$ORIGIN"]),
            ),
            (
                format!("{g}/gconv/libKSC.so"),
                readelf("DYN", None, Some("libKSC.so"), &["libc.so.6"], &[]),
            ),
            // Never loaded by the PostgreSQL output: not a root.
            (
                format!("/td/store/{GLIBC}/stage/td/store/glibc-2.41-x86_64/bin/zdump"),
                readelf("DYN", Some(&loader), None, &["libc.so.6"], &[]),
            ),
            (format!("{p}/libpq.a"), readelf("REL", None, None, &[], &[])),
        ]
    };
    // The loader opens a candidate if a file is there; libpq.so.5 is a link.
    let find = |candidate: &str| -> R<Option<String>> {
        let target = if candidate == format!("{p}/libpq.so.5") {
            format!("{p}/libpq.so.5.17")
        } else {
            candidate.to_owned()
        };
        let present = files(&[], &[]).iter().any(|(path, _)| *path == target)
            || target == format!("{p}/README");
        Ok(present.then_some(target))
    };
    let check = |files: &[(String, String)]| {
        let objects: Vec<(&str, closure::Elf)> = files
            .iter()
            .map(|(path, report)| (path.as_str(), closure::parse(report).unwrap()))
            .collect();
        closure::check_loading(&objects, PG, &find)
    };
    let good = files(&["libpq.so.5", "libm.so.6", "libc.so.6"], &[&g]);
    check(&good).unwrap();

    let refused = |files: Vec<(String, String)>, why: &str| {
        let err = check(&files).unwrap_err().to_string();
        assert!(err.contains(why), "{err}");
    };
    // A module without a run path to td glibc would reach the host's libc.
    refused(
        files(&["libpq.so.5", "libc.so.6"], &[]),
        "pg_trgm.so needs libc.so.6",
    );
    // td glibc's objects carry no run path: libresolv only resolves if the
    // root's own run path provides it before something needs it.
    refused(
        files(&["libpq.so.5", "libc.so.6", "libfoo.so.1"], &[&g]),
        "postgres needs libfoo.so.1",
    );
    let mut needs_resolv = good.clone();
    needs_resolv[6].1 = readelf(
        "DYN",
        None,
        Some("libm.so.6"),
        &["libc.so.6", "libresolv.so.2"],
        &[],
    );
    refused(needs_resolv, "libm.so.6 needs libresolv.so.2");
    // An object the run path names that is not an ELF object of the closure.
    let mut readme = good.clone();
    readme[3].1 = readelf("DYN", None, None, &["README"], &[&p]);
    refused(readme, "README is not an ELF object");
    // $ORIGIN resolves next to the object, and a name loaded already
    // satisfies a later NEEDED without a run path.
    let mut gconv = good.clone();
    gconv[3].1 = readelf(
        "DYN",
        None,
        None,
        &["libc.so.6", "ISO-2022-KR.so"],
        &[&g, &format!("{g}/gconv")],
    );
    check(&gconv).unwrap();
    gconv[8].1 = readelf("DYN", None, None, &["libKSC.so", "libc.so.6"], &[]);
    refused(gconv, "ISO-2022-KR.so needs libKSC.so");
    // Nothing in the PostgreSQL output is loadable.
    let none: Vec<(String, String)> = good
        .into_iter()
        .filter(|(path, _)| !path.starts_with(&format!("/td/store/{PG}/")))
        .collect();
    refused(none, "loads nothing");
}

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

const TARGET_ARGS: &[&str] = &[
    "--host",
    "192.0.2.10",
    "--host-name",
    "mx1",
    "--account",
    "alice",
    "--identity",
    "~/.ssh/deploy",
    "--jmap-account",
    "alice",
    "--jmap-url",
    "https://mx.example.org/.well-known/jmap",
];

#[test]
fn the_file_commands_take_each_field_once_by_flag() {
    let args = strings(TARGET_ARGS);
    let v = files::values(&args, crate::target::TARGET_FLAGS, "all").unwrap();
    assert_eq!((v.config, v.replace), (None, false));
    assert_eq!(v.fields.len(), 6);
    assert!(v.fields.contains(&("identity", "~/.ssh/deploy")));
    let mut more = args.clone();
    more.extend(strings(&["--replace", "--config", "/tmp/t.json"]));
    let v = files::values(&more, crate::target::TARGET_FLAGS, "all").unwrap();
    assert_eq!((v.config, v.replace), (Some("/tmp/t.json"), true));
    for refused in [
        strings(&TARGET_ARGS[2..]),
        strings(&[TARGET_ARGS, &["--host", "192.0.2.11"]].concat()),
        strings(&[TARGET_ARGS, &["--replace", "--replace"]].concat()),
        strings(&[TARGET_ARGS, &["--config"]].concat()),
        strings(&[TARGET_ARGS, &["--port", "22"]].concat()),
        strings(&TARGET_ARGS[..TARGET_ARGS.len() - 1]),
        // An option is never taken for the value before it.
        strings(&[&TARGET_ARGS[..TARGET_ARGS.len() - 1], &["--replace"]].concat()),
        strings(&[TARGET_ARGS, &["--config", "--replace"]].concat()),
    ] {
        assert!(
            files::values(&refused, crate::target::TARGET_FLAGS, "all").is_err(),
            "{refused:?}"
        );
    }
    let host = strings(&[
        "--build-account",
        "alice",
        "--build-home",
        "/home/alice",
        "--public-name",
        "mx.example.org",
    ]);
    let v = files::values(&host, crate::target::HOST_FLAGS, "all").unwrap();
    assert_eq!(v.fields.len(), 3);
    // host_name comes from target.json, never from a flag.
    let named = [&host[..], &strings(&["--host-name", "mx1"])].concat();
    assert!(files::values(&named, crate::target::HOST_FLAGS, "all").is_err());
}

#[test]
fn write_target_creates_keeps_and_replaces_only_on_request() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let scratch = TempDir::new("deploy-write-target").unwrap();
    let home = scratch.path().join("home");
    let args = strings(TARGET_ARGS);
    let v = files::values(&args, crate::target::TARGET_FLAGS, "all").unwrap();
    let (text, target) = Target::render(&v.fields, &home).unwrap();
    let path = crate::target::target_path(None, &home, None).unwrap();
    assert_eq!(path, home.join(".config/td-mta-deploy/target.json"));
    // The default directory is created private; an explicit file's is not.
    let explicit = scratch.path().join("absent/target.json");
    assert!(files::store_target(&explicit, &text, &target, &home, false, false).is_err());
    assert_eq!(
        files::store_target(&path, &text, &target, &home, false, true).unwrap(),
        "target-written"
    );
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o600);
    assert_eq!(
        fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o7777,
        0o700
    );
    assert_eq!(Target::load(None, &home, None).unwrap(), target);
    assert_eq!(
        files::store_target(&path, &text, &target, &home, false, true).unwrap(),
        "target-unchanged"
    );
    let other = args
        .iter()
        .map(|a| a.replace("192.0.2.10", "192.0.2.20"))
        .collect::<Vec<_>>();
    let w = files::values(&other, crate::target::TARGET_FLAGS, "all").unwrap();
    let (new_text, new_target) = Target::render(&w.fields, &home).unwrap();
    let refused = files::store_target(&path, &new_text, &new_target, &home, false, true);
    assert!(refused.unwrap_err().to_string().contains("--replace"));
    assert_eq!(Target::load(None, &home, None).unwrap(), target);
    assert_eq!(
        files::store_target(&path, &new_text, &new_target, &home, true, true).unwrap(),
        "target-replaced"
    );
    assert_eq!(Target::load(None, &home, None).unwrap(), new_target);
    // No staging file is left beside it.
    let names: Vec<_> = fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["target.json"]);
    // The same values in a file others can read are rewritten private.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        files::store_target(&path, &new_text, &new_target, &home, false, true).unwrap(),
        "target-repaired"
    );
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o600);
    // An unreadable or invalid file is replaced only on request too.
    fs::write(&path, "{}").unwrap();
    let refused = files::store_target(&path, &text, &target, &home, false, true);
    assert_eq!(refused.unwrap_err().to_string(), files::DIFFERS);
    assert_eq!(fs::read(&path).unwrap(), b"{}");
    // A bare relative --config names a file in the working directory.
    assert_eq!(
        crate::target::target_path(Some(Path::new("t.json")), &home, None).unwrap(),
        Path::new("t.json")
    );
    // The key must be a private regular file.
    let key = scratch.path().join("key");
    assert!(files::private_key(&key).is_err());
    fs::write(&key, "k").unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(files::private_key(&key).is_err());
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    files::private_key(&key).unwrap();
    assert!(files::private_key(scratch.path()).is_err());
}

#[test]
fn host_json_is_written_only_where_absent_wrong_or_replaced_on_request() {
    use files::{host_decision, Existing};
    let state = |same, private| Existing { same, private };
    assert_eq!(host_decision(None, false).unwrap(), "host-provisioned");
    assert_eq!(
        host_decision(Some(&state(true, true)), false).unwrap(),
        "host-unchanged"
    );
    assert_eq!(
        host_decision(Some(&state(true, true)), true).unwrap(),
        "host-unchanged"
    );
    assert_eq!(
        host_decision(Some(&state(true, false)), false).unwrap(),
        "host-repaired"
    );
    for private in [true, false] {
        let differs = host_decision(Some(&state(false, private)), false);
        assert_eq!(differs.unwrap_err().to_string(), files::DIFFERS);
        assert_eq!(
            host_decision(Some(&state(false, private)), true).unwrap(),
            "host-replaced"
        );
    }
}

#[test]
fn the_outcome_line_is_one_json_object_after_a_fixed_prefix() {
    let line = outcome_line(&Outcome {
        result: "target-written",
        error: None,
        next: Some(Next {
            argv: &["td-mta/deploy", "--provision-host", "--config", "t.json"],
            needs: &["--build-account"],
        }),
    })
    .unwrap();
    let json = line
        .strip_prefix("td-mta-deploy-outcome ")
        .and_then(|rest| rest.strip_suffix('\n'))
        .unwrap();
    assert!(!json.contains('\n'));
    let json = td_json::parse(json).unwrap();
    assert_eq!(
        json.get("result").and_then(Json::as_str),
        Some("target-written")
    );
    assert!(json.get("error").is_none());
    let next = json.get("next").unwrap();
    assert_eq!(next.get("argv").and_then(Json::as_arr).unwrap().len(), 4);
    let refused = outcome_line(&Outcome {
        result: "refused-differs",
        error: Some(files::DIFFERS),
        next: None,
    })
    .unwrap();
    let json = td_json::parse(
        refused
            .trim()
            .strip_prefix("td-mta-deploy-outcome ")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        json.get("error").and_then(Json::as_str),
        Some(files::DIFFERS)
    );
    assert!(json.get("next").unwrap().is_null());
}

#[test]
fn the_build_account_is_unprivileged_and_owns_the_build_home() {
    let entry = "alice:x:1000:1000:Alice:/home/alice:/bin/bash";
    files::passwd_home(entry, "/home/alice").unwrap();
    for (entry, home) in [
        (entry, "/home/bob"),
        ("root:x:0:0:root:/home/alice:/bin/sh", "/home/alice"),
        ("alice:x:1000:1000:Alice:/home/alice", "/home/alice"),
        (
            "alice:x:1000:1000:Alice:/home/alice:/bin/sh:x",
            "/home/alice",
        ),
        ("alice:x:u:1000:Alice:/home/alice:/bin/sh", "/home/alice"),
    ] {
        assert!(files::passwd_home(entry, home).is_err(), "{entry}");
    }
}
