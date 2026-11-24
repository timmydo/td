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
        ssh_options: Vec::new(),
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
        ssh_options: Vec::new(),
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
fn retiring_rollback_material_needs_irreversible_and_runs_alone() {
    let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let retire = args(&["--retire-rollback", "--irreversible"]);
    let opts = parse(&retire).unwrap();
    assert!(opts.retire && opts.irreversible && !opts.socket && !opts.binaries);
    let rev = "0123456789abcdef0123456789abcdef01234567";
    let deploy = args(&["--irreversible", "--schema-target", "7", "--revision", rev]);
    let opts = parse(&deploy).unwrap();
    assert!(opts.irreversible && opts.schema_target == Some(7) && opts.revision == Some(rev));
    let upper = rev.to_ascii_uppercase();
    for refused in [
        // A deployment authorises one named migration, retirement none.
        args(&["--irreversible"]),
        args(&["--schema-target", "7"]),
        args(&["--irreversible", "--schema-target", "7"]),
        args(&["--schema-target", "7", "--revision", rev]),
        args(&[
            "--irreversible",
            "--schema-target",
            "seven",
            "--revision",
            rev,
        ]),
        args(&["--irreversible", "--schema-target", "+7", "--revision", rev]),
        args(&[
            "--irreversible",
            "--schema-target",
            "7",
            "--revision",
            &upper,
        ]),
        args(&[
            "--irreversible",
            "--schema-target",
            "7",
            "--revision",
            "abc",
        ]),
        args(&["--irreversible", "--schema-target"]),
        args(&[
            "--retire-rollback",
            "--irreversible",
            "--schema-target",
            "7",
        ]),
        args(&["--retire-rollback", "--irreversible", "--revision", rev]),
        args(&["--retire-rollback"]),
        args(&["--retire-rollback", "--irreversible", "--skip-tests"]),
        args(&[
            "--retire-rollback",
            "--irreversible",
            "--window-seconds",
            "60",
        ]),
        args(&["--retire-rollback", "--irreversible", "--postgres-binaries"]),
        args(&["--postgres-binaries", "--irreversible"]),
    ] {
        assert!(parse(&refused).is_err(), "{refused:?}");
    }
    // Without a schema target only a stepping stone, whose controller
    // reads none, is authorised; known once the transport is planned.
    let stone = args(&["--irreversible", "--revision", rev]);
    let opts = parse(&stone).unwrap();
    assert!(named_migration(&opts, true).is_ok());
    assert!(named_migration(&opts, false).is_err());
    let full = args(&["--irreversible", "--schema-target", "7", "--revision", rev]);
    assert!(named_migration(&parse(&full).unwrap(), false).is_ok());
}

#[test]
fn recovery_runs_alone() {
    let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let alone = args(&["--recover"]);
    let opts = parse(&alone).unwrap();
    assert!(opts.recover && !opts.retire && !opts.irreversible);
    for other in [
        "--skip-tests",
        "--postgres-socket",
        "--postgres-binaries",
        "--retire-rollback",
        "--irreversible",
    ] {
        assert!(parse(&args(&["--recover", other])).is_err(), "{other}");
    }
    assert!(parse(&args(&["--recover", "--window-seconds", "60"])).is_err());
}

#[test]
fn a_rollback_is_its_own_binary_only_activation() {
    let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let alone = args(&["--rollback", "--window-seconds", "120"]);
    let opts = parse(&alone).unwrap();
    assert!(opts.rollback && opts.budget == 120 && !opts.irreversible);
    for other in [
        "--skip-tests",
        "--postgres-socket",
        "--postgres-binaries",
        "--retire-rollback",
        "--recover",
        "--irreversible",
    ] {
        assert!(parse(&args(&["--rollback", other])).is_err(), "{other}");
    }
}

#[test]
fn the_binary_switch_takes_no_attestation() {
    let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let only = args(&["--postgres-binaries"]);
    let opts = parse(&only).unwrap();
    assert!(opts.binaries && !opts.socket && !opts.to_debian);
    let back = args(&["--postgres-binaries", "--to-debian"]);
    let back = parse(&back).unwrap();
    assert!(back.binaries && back.to_debian);
    for refused in [
        args(&["--to-debian"]),
        args(&["--postgres-socket", "--to-debian"]),
        args(&["--postgres-binaries", "--to-debian", "--irreversible"]),
        args(&["--postgres-binaries", "--postgres-minor-reviewed", "17.6"]),
        args(&["--postgres-binaries", "--skip-tests"]),
        args(&["--postgres-binaries", "--postgres-socket"]),
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
        host_key: Some("host-key-enrolled"),
        error: None,
        details: None,
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
        host_key: None,
        error: Some(files::DIFFERS),
        details: None,
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

#[test]
fn a_host_key_is_enrolled_only_with_the_given_fingerprint() {
    use host_key::{decision, fingerprint, line, scanned, Key, KEY_DIFFERS};
    let good = "SHA256:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU";
    assert_eq!(fingerprint(good).unwrap(), good);
    for refused in [
        "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU",
        "SHA256:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuF",
        "SHA256:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=",
        "MD5:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU",
    ] {
        assert!(fingerprint(refused).is_err(), "{refused}");
    }
    let scan =
        "# mx1:22 SSH-2.0-OpenSSH_9.6\nmx1 ssh-ed25519 AAAAC3k\nmx1 ecdsa-sha2-nistp256 AAAAE2v\n";
    let keys = scanned(scan, "mx1").unwrap();
    assert_eq!(
        keys.first(),
        Some(&Key {
            kind: "ssh-ed25519",
            blob: "AAAAC3k"
        })
    );
    assert_eq!(keys.len(), 2);
    assert!(scanned("other ssh-ed25519 AAAA\n", "mx1").is_err());
    let wanted = line("mx1", &keys[0]).unwrap();
    assert_eq!(wanted, "mx1 ssh-ed25519 AAAAC3k\n");
    assert_eq!(decision(None, &wanted, false).unwrap(), "host-key-enrolled");
    assert_eq!(
        decision(Some(&wanted), &wanted, false).unwrap(),
        "host-key-unchanged"
    );
    let other = "mx1 ssh-rsa AAAAB3\n";
    let refused = decision(Some(other), &wanted, false)
        .unwrap_err()
        .to_string();
    assert_eq!(refused, KEY_DIFFERS);
    assert_eq!(
        decision(Some(other), &wanted, true).unwrap(),
        "host-key-replaced"
    );
    let args = strings(&["--host-key-sha256", good, "--host-key-sha256", good]);
    assert!(files::values(&args, &[], "x").is_err());
    let args = strings(&["--host-key-sha256", "--replace"]);
    assert!(files::values(&args, &[], "x").is_err());
}

#[test]
fn the_plan_picks_the_offered_key_with_the_fingerprint_before_writing() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new("host-key-plan").unwrap();
    let target = dir.path().join("target.json");
    let file = host_key::known_hosts(&target).unwrap();
    let (ed, ec) = (
        "SHA256:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU",
        "SHA256:ypeBEsobvcr6wjGzmiPcTaeG7/gUfE5yuYB3ha/uSLs",
    );
    let offered =
        "# mx1:22 SSH-2.0-OpenSSH_9.6\nmx1 ssh-ed25519 AAAAC3k\nmx1 ecdsa-sha2-nistp256 AAAAE2v\n";
    let scan = |out: &'static str| {
        move |host: &str| -> R<Vec<u8>> {
            assert_eq!(host, "mx1");
            Ok(out.as_bytes().to_vec())
        }
    };
    // What ssh-keygen -l prints for each line the host offered.
    let print = |line: &str| -> R<Vec<u8>> {
        let printed = match line {
            "mx1 ssh-ed25519 AAAAC3k\n" => format!("256 {ed} mx1 (ED25519)\n"),
            "mx1 ecdsa-sha2-nistp256 AAAAE2v\n" => format!("256 {ec} mx1 (ECDSA)\n"),
            other => return Err(format!("unexpected line {other:?}").into()),
        };
        Ok(printed.into_bytes())
    };
    let plan = |expected, offered, replace| {
        host_key::Enrollment::plan_with("mx1", expected, &target, replace, scan(offered), print)
    };
    let refused = |r: R<host_key::Enrollment>| r.err().map(|e| e.to_string()).unwrap();
    let chosen = plan(ec, offered, false).unwrap();
    assert_eq!(chosen.line, "mx1 ecdsa-sha2-nistp256 AAAAE2v\n");
    assert_eq!(chosen.result, "host-key-enrolled");
    assert_eq!(chosen.file, file);
    // Planning writes nothing.
    assert!(fs::symlink_metadata(&file).is_err());
    let missing = "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let refusal = refused(plan(missing, offered, false));
    assert!(refusal.contains("none of the keys"), "{refusal}");
    let none = refused(plan(ed, "# mx1:22 SSH-2.0-OpenSSH_9.6\n", false));
    assert!(none.contains("offered no SSH key"), "{none}");
    assert!(
        host_key::Enrollment::plan_with("MX1", ed, &target, false, scan(offered), print).is_err()
    );
    // An enrolled file decides unchanged, repaired or, with the other
    // key, refused unless replaced.
    fs::write(&file, "mx1 ssh-ed25519 AAAAC3k\n").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        plan(ed, offered, false).unwrap().result,
        "host-key-unchanged"
    );
    let differs = refused(plan(ec, offered, false));
    assert_eq!(differs, host_key::KEY_DIFFERS);
    assert_eq!(plan(ec, offered, true).unwrap().result, "host-key-replaced");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        plan(ed, offered, false).unwrap().result,
        "host-key-repaired"
    );
    // A known_hosts path that is no regular file is refused.
    fs::remove_file(&file).unwrap();
    fs::create_dir(&file).unwrap();
    assert!(refused(plan(ed, offered, true)).contains("not a regular file"));
}

#[test]
fn the_enrolled_key_lives_beside_the_target_and_pins_ssh() {
    let dir = TempDir::new("known-hosts").unwrap();
    let target = dir.path().join("target.json");
    let file = host_key::known_hosts(&target).unwrap();
    assert_eq!(file, dir.path().join("target.json.known_hosts"));
    // Nothing enrolled: ssh's own known_hosts, as before.
    assert!(host_key::ssh_options(&target).unwrap().is_empty());
    let enroll = |result| host_key::Enrollment {
        line: "mx1 ssh-ed25519 AAAAC3k\n".into(),
        file: file.clone(),
        result,
    };
    enroll("host-key-enrolled").apply().unwrap();
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "mx1 ssh-ed25519 AAAAC3k\n"
    );
    let mode = fs::metadata(&file).unwrap().permissions();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
        0o600
    );
    let options = host_key::ssh_options(&target).unwrap();
    assert_eq!(
        options,
        [
            format!("-oUserKnownHostsFile={}", file.display()),
            "-oGlobalKnownHostsFile=/dev/null".into(),
            "-oUpdateHostKeys=no".into(),
            "-oCheckHostIP=no".into(),
            "-oVerifyHostKeyDNS=no".into(),
        ]
    );
    // Enrolling where a key is already enrolled needs the replace path.
    assert!(enroll("host-key-enrolled").apply().is_err());
    let replaced = host_key::Enrollment {
        line: "mx1 ssh-rsa AAAAB3\n".into(),
        file: file.clone(),
        result: "host-key-replaced",
    };
    replaced.apply().unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), "mx1 ssh-rsa AAAAB3\n");
    enroll("host-key-unchanged").apply().unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), "mx1 ssh-rsa AAAAB3\n");
    // The right key in a file others can read is rewritten privately.
    let open = std::os::unix::fs::PermissionsExt::from_mode(0o644);
    fs::set_permissions(&file, open).unwrap();
    enroll("host-key-repaired").apply().unwrap();
    let mode = fs::metadata(&file).unwrap().permissions();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
        0o600
    );
}

#[test]
fn only_a_private_key_file_at_a_literal_path_pins_ssh() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new("known-hosts-checks").unwrap();
    let target = dir.path().join("t.json");
    let file = host_key::known_hosts(&target).unwrap();
    fs::write(&file, "mx1 ssh-ed25519 AAAAC3k\n").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    // A file the tool did not write, readable by others, is refused.
    assert!(host_key::ssh_options(&target).is_err());
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(host_key::ssh_options(&target).unwrap().len(), 5);
    // A link to a private file is not the file the tool wrote.
    let other = dir.path().join("other");
    fs::rename(&file, &other).unwrap();
    std::os::unix::fs::symlink(&other, &file).unwrap();
    assert!(host_key::ssh_options(&target).is_err());
    for bad in [
        "a b/t.json",
        "%h/t.json",
        "$X/t.json",
        "~root/t.json",
        "q\"/t.json",
        "a\\\\b/t.json",
    ] {
        assert!(
            host_key::known_hosts(&dir.path().join(bad)).is_err(),
            "{bad}"
        );
    }
    assert!(host_key::known_hosts(Path::new("relative.json"))
        .unwrap()
        .is_absolute());
}

#[test]
fn the_live_drop_in_reads_back_as_the_controller_wrote_it() {
    const OLD: &str = "a1f6m76fp7k1scqd064a892wz5d4aqcq-postgresql-x86-64-17.6";
    let program = format!("/td/store/{OLD}/bin/postgres");
    let written =
        crate::activate::binaries::dropin_text(&program, &["-D", "/var/lib/td-mta/mail"]).unwrap();
    // What the SSH command printed, trimmed, reads back as written.
    let printed = remote_text(written.trim()).unwrap();
    assert_eq!(printed, written);
    assert_eq!(
        crate::activate::binaries::dropin_program(&printed).unwrap(),
        program
    );
    assert_eq!(remote_text("").unwrap(), "\n");
}

/// A run's arguments and what they parse to.
fn run_of(args: &[&str]) -> (&'static [String], R<Options<'static>>) {
    let owned: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let leaked: &'static [String] = Vec::leak(owned);
    (leaked, parse(leaked))
}

fn options(args: &[&str]) -> R<Options<'static>> {
    run_of(args).1
}

#[test]
fn the_restore_and_the_refresh_are_jobs_of_their_own() {
    let one = |args: &[&str]| options(args).is_ok();
    assert!(options(&["--restore-shared-services"]).unwrap().restore);
    assert!(one(&[
        "--restore-shared-services",
        "--window-seconds",
        "90",
        "--jmap-account",
        "a"
    ]));
    assert!(
        options(&["--refresh-notifications", "--config", "/t.json"])
            .unwrap()
            .refresh
    );
    for args in [
        &["--restore-shared-services", "--rollback"][..],
        &["--restore-shared-services", "--irreversible"],
        &["--restore-shared-services", "--refresh-notifications"],
        &["--refresh-notifications", "--window-seconds", "90"],
        &["--refresh-notifications", "--jmap-account", "a"],
        &["--refresh-notifications", "--recover"],
        &["--restore-shared-services", "--converge-config"],
        &["--refresh-notifications", "--converge-config"],
    ] {
        assert!(!one(args), "{args:?}");
    }
    // An unknown option is named, not answered with the whole help.
    let unknown = options(&["--restore"]).err().unwrap().to_string();
    assert_eq!(
        unknown,
        "unknown option --restore: td-mta/deploy --help lists them"
    );
}

#[test]
fn a_refusal_names_the_command_it_asks_for() {
    assert_eq!(
        named("PostgreSQL still uses loopback TLS: run td-mta/deploy --postgres-socket"),
        Some(vec!["--postgres-socket"])
    );
    assert_eq!(
        named("too slow: run td-mta/deploy --postgres-binaries --window-seconds 1320"),
        Some(vec!["--postgres-binaries", "--window-seconds", "1320"])
    );
    assert_eq!(
        named("x: run td-mta/deploy --postgres-binaries --to-debian again"),
        Some(vec!["--postgres-binaries", "--to-debian"])
    );
    assert_eq!(named("run td-mta/deploy 5 --x"), None);
    // The controller's own wording on loopback TLS names plain deploy.
    assert_eq!(
        named("PostgreSQL still uses loopback TLS: run td-mta/deploy, which names the next step"),
        Some(vec![])
    );
    assert_eq!(named("wait, then run td-mta/deploy again"), None);
    assert_eq!(named("production release changed"), None);
    // The controller's refusal reads back, whatever migrates.
    let rev = "ab".repeat(20);
    for (before, target, identities) in [(3, 4, false), (4, 4, true), (4, 4, false)] {
        let refusal = crate::activate::migrates(before, target, identities, &rev).unwrap();
        assert!(refusal.starts_with(crate::activate::MIGRATES));
        assert_eq!(named(&refusal), None);
        let target = target.to_string();
        assert_eq!(migration(&refusal), Some((target.as_str(), rev.as_str())));
    }
    let unchanged = crate::activate::migrates(4, 4, false, &rev).unwrap();
    assert!(unchanged.contains("schema 4 unchanged; grants change"));
    let identity = crate::activate::migrates(4, 4, true, &rev).unwrap();
    assert!(identity.contains("schema 4 unchanged, isolating the service identities"));
    // Main moved past the authorised revision: rerun without it.
    let moved: Vec<String> = [
        "--irreversible",
        "--schema-target",
        "4",
        "--revision",
        &rev,
        "--config",
        "/t.json",
    ]
    .iter()
    .map(|a| a.to_string())
    .collect();
    assert_eq!(
        refused_next(MOVED, &moved).unwrap().unwrap(),
        ["td-mta/deploy", "--config", "/t.json"]
    );
    assert!(refused_next("other", &moved).unwrap().is_none());
    // A run that skipped tests is offered again only for approval.
    let skipped: Vec<String> = [
        "--skip-tests",
        "--irreversible",
        "--schema-target",
        "4",
        "--revision",
        &rev,
    ]
    .iter()
    .map(|a| a.to_string())
    .collect();
    let end = held(Ended {
        result: "refused",
        error: Some(MOVED.to_string()),
        next: refused_next(MOVED, &skipped).unwrap(),
        approve: None,
        not_before: None,
    });
    assert!(end.next.is_none());
    let (argv, needs) = end.approve.unwrap();
    assert_eq!(argv, ["td-mta/deploy", "--skip-tests"]);
    assert_eq!(needs, ["skip-tests-approval"]);
    for broken in [
        "run td-mta/deploy --irreversible --schema-target 4",
        "run td-mta/deploy --irreversible --schema-target 4 --revision abc",
        "run td-mta/deploy --irreversible --schema-target x --revision 0000000000000000000000000000000000000000",
    ] {
        assert_eq!(migration(broken), None, "{broken}");
    }
    // Only the authorisation is dropped, not values that look like it.
    let given: Vec<String> = [
        "--config",
        "--irreversible",
        "--irreversible",
        "--schema-target",
        "3",
        "--revision",
        &rev,
        "--skip-tests",
    ]
    .iter()
    .map(|a| a.to_string())
    .collect();
    assert_eq!(
        unauthorised(&given).unwrap(),
        ["--config", "--irreversible", "--skip-tests"]
    );
    assert_eq!(
        named("cannot be undone: run td-mta/deploy --retire-rollback --irreversible"),
        None
    );
}

#[test]
fn a_job_outcome_names_what_follows_from_its_end() {
    let (retire_args, retire) =
        run_of(&["--retire-rollback", "--irreversible", "--config", "/t.json"]);
    let retire = retire.unwrap();
    let (deploy_args, deploy) = run_of(&["--skip-tests", "--window-seconds", "90"]);
    let deploy = deploy.unwrap();
    let (recover_args, recover) = run_of(&["--recover"]);
    let recover = recover.unwrap();
    let (restore_args, restore) = run_of(&["--restore-shared-services"]);
    let restore = restore.unwrap();
    let job = |exit: Option<i32>, record: &str, fence: Option<bool>, post: Option<bool>| Job {
        receipt: "/var/lib/td-mta-operator/deploy-abcdefgh/receipt.json".to_string(),
        record: td_json::parse(record).ok(),
        exit,
        fence,
        post,
        stepping: false,
    };
    let at_deploy = |j: &Job| ended(&deploy, deploy_args, j).unwrap();
    let argv = |end: &Ended| end.next.clone().unwrap_or_default().join(" ");
    // A command that takes a decision is offered only as `approve`.
    let approval = |end: &Ended| {
        assert!(end.next.is_none());
        let (argv, needs) = end.approve.clone().unwrap();
        (argv.join(" "), needs)
    };
    let verified = job(
        Some(0),
        r#"{"status":"verified deployment"}"#,
        Some(false),
        Some(true),
    );
    let done = at_deploy(&verified);
    assert_eq!(done.result, "succeeded");
    assert!(done.next.is_none() && done.error.is_none());
    let mut written = Vec::new();
    write_job_outcome(&mut written, &done, &verified).unwrap();
    let line = String::from_utf8(written).unwrap();
    let json = line
        .trim()
        .strip_prefix("td-mta-deploy-outcome ")
        .map(td_json::parse)
        .unwrap()
        .unwrap();
    assert_eq!(
        json.get("status").and_then(Json::as_str),
        Some("verified deployment")
    );
    assert_eq!(
        json.get("receipt").and_then(Json::as_str),
        Some(verified.receipt.as_str())
    );
    assert!(json.get("next").unwrap().is_null());
    // Without a record read, the outcome names no record.
    let silent = job(Some(1), "", Some(false), Some(true));
    let mut written = Vec::new();
    write_job_outcome(&mut written, &at_deploy(&silent), &silent).unwrap();
    let line = String::from_utf8(written).unwrap();
    assert!(!line.contains("\"receipt\""), "{line}");
    // A failed public check after a finished activation leaves the
    // rollback to the operator; after another job it keeps what follows
    // that job.
    let unchecked = at_deploy(&job(Some(0), "{}", Some(false), Some(false)));
    assert_eq!(unchecked.result, "check-failed");
    assert!(unchecked.next.is_none());
    assert!(unchecked.error.unwrap().contains("--rollback"));
    let restored = ended(
        &restore,
        restore_args,
        &job(Some(0), "{}", Some(false), Some(false)),
    )
    .unwrap();
    assert_eq!(restored.result, "check-failed");
    assert!(restored.next.is_none());
    // A held fence and an unknown end are followed by recovery, a
    // recovery's own too.
    let at_recover = |j: &Job| ended(&recover, recover_args, j).unwrap();
    for end in [
        at_deploy(&job(Some(1), r#"{"error":"x"}"#, Some(true), Some(false))),
        at_deploy(&job(Some(255), "{}", Some(false), Some(false))),
        at_deploy(&job(None, "{}", None, None)),
        at_deploy(&job(Some(1), r#"{"error":"x"}"#, None, Some(true))),
        at_recover(&job(Some(255), "{}", None, Some(true))),
        at_recover(&job(
            Some(1),
            r#"{"error":"timed out"}"#,
            Some(true),
            Some(true),
        )),
    ] {
        assert_eq!(argv(&end), "td-mta/deploy --recover");
        assert!(end.error.is_some());
    }
    let unknown = at_deploy(&job(Some(255), "{}", None, None));
    assert_eq!(unknown.result, "unknown");
    // A fence recovery refuses by its kind is not rerun, and a recovery
    // that left no fence does not follow its own refusal.
    for refusal in crate::activate::UNRECOVERABLE {
        let record = format!(r#"{{"error":"{refusal}"}}"#);
        let end = at_recover(&job(Some(1), &record, Some(true), Some(true)));
        assert_eq!((end.result, end.next.is_none()), ("failed", true));
    }
    let fenced = r#"{"error":"unfinished deployment fence: run td-mta/deploy --recover"}"#;
    assert!(at_recover(&job(Some(1), fenced, Some(false), Some(true)))
        .next
        .is_none());
    let locked = format!(r#"{{"status":"refused","error":"{}"}}"#, crate::LOCKED);
    let waits = at_recover(&job(Some(1), &locked, Some(true), Some(true)));
    assert_eq!(waits.result, "busy");
    assert_eq!(argv(&waits), "td-mta/deploy --recover");
    assert!(waits.approve.is_none());
    // Another job holding the host: this one is repeated as it was, and
    // skipping its tests again is the operator's decision.
    let busy = at_deploy(&job(Some(1), &locked, Some(true), Some(true)));
    assert_eq!(busy.result, "busy");
    assert_eq!(
        approval(&busy),
        (
            "td-mta/deploy --skip-tests --window-seconds 90".to_string(),
            vec!["skip-tests-approval"]
        )
    );
    // The migration refusal is a decision the next command takes; it
    // keeps the run's own options and authorises exactly that migration,
    // replacing one the run authorised that the release does not make.
    let rev = "c".repeat(40);
    let message = crate::activate::migrates(3, 4, false, &rev).unwrap();
    let migrates = format!(r#"{{"error":"{message}"}}"#);
    let refused = at_deploy(&job(Some(1), &migrates, Some(false), Some(true)));
    assert_eq!(refused.result, "refused-irreversible");
    let authorised = format!(
        "td-mta/deploy --skip-tests --window-seconds 90 --irreversible --schema-target 4 --revision {rev}"
    );
    let both = vec!["irreversible-approval", "skip-tests-approval"];
    assert_eq!(approval(&refused), (authorised.clone(), both.clone()));
    let mut written = Vec::new();
    write_job_outcome(
        &mut written,
        &refused,
        &job(Some(1), &migrates, Some(false), Some(true)),
    )
    .unwrap();
    let line = String::from_utf8(written).unwrap();
    let json = line
        .trim()
        .strip_prefix("td-mta-deploy-outcome ")
        .map(td_json::parse)
        .unwrap()
        .unwrap();
    assert!(json.get("next").unwrap().is_null());
    let approve = json.get("approve").unwrap();
    assert_eq!(
        approve.get("needs").unwrap().to_string(),
        r#"["irreversible-approval","skip-tests-approval"]"#
    );
    assert_eq!(
        approve
            .get("argv")
            .and_then(Json::as_array)
            .map(<[Json]>::len),
        Some(9)
    );
    let stale_rev = "d".repeat(40);
    let (stale_args, stale) = run_of(&[
        "--irreversible",
        "--schema-target",
        "3",
        "--revision",
        &stale_rev,
        "--skip-tests",
        "--window-seconds",
        "90",
    ]);
    let stale = ended(
        &stale.unwrap(),
        stale_args,
        &job(Some(1), &migrates, Some(false), Some(true)),
    )
    .unwrap();
    assert_eq!(stale.result, "refused-irreversible");
    assert_eq!(approval(&stale), (authorised, both));
    // Retirement inside the window, after it with material left, and
    // keeping the Debian tree.
    let window = format!(
        r#"{{"error":"{}","rollback_retire_not_before":1700000000}}"#,
        crate::activate::INSIDE_WINDOW
    );
    let at_retire = |j: &Job| ended(&retire, retire_args, j).unwrap();
    let deferred = at_retire(&job(Some(1), &window, Some(false), Some(true)));
    assert_eq!(deferred.result, "deferred");
    assert_eq!(deferred.not_before, Some(1_700_000_000));
    let retirement = (
        "td-mta/deploy --retire-rollback --irreversible --config /t.json".to_string(),
        vec!["irreversible-approval"],
    );
    assert_eq!(approval(&deferred), retirement);
    let left = r#"{"rollback_retire_not_before":1700000000}"#;
    let partial = at_retire(&job(Some(0), left, Some(false), Some(true)));
    assert_eq!(partial.result, "succeeded");
    assert_eq!(partial.not_before, Some(1_700_000_000));
    assert_eq!(approval(&partial), retirement);
    let checked = at_retire(&job(Some(0), left, Some(false), Some(false)));
    assert_eq!(checked.result, "check-failed");
    assert_eq!(checked.not_before, Some(1_700_000_000));
    assert_eq!(approval(&checked), retirement);
    let tree = r#"{"debian_tree_kept":"why"}"#;
    let kept = at_retire(&job(Some(0), tree, Some(false), Some(true)));
    assert_eq!(
        argv(&kept),
        "td-mta/deploy --postgres-binaries --config /t.json"
    );
    // A refusal naming a command, whether recorded in the receipt or
    // before it, is followed by that command; any other names none.
    let socket = r#"{"status":"refused","error":"PostgreSQL still uses loopback TLS: run td-mta/deploy --postgres-socket"}"#;
    let named = at_deploy(&job(Some(1), socket, Some(false), Some(true)));
    assert_eq!(named.result, "failed");
    assert_eq!(argv(&named), "td-mta/deploy --postgres-socket");
    let tls = r#"{"status":"refused","error":"PostgreSQL still uses loopback TLS: run td-mta/deploy, which names the next step"}"#;
    let plain = at_deploy(&job(Some(1), tls, Some(false), Some(true)));
    assert_eq!(plain.result, "failed");
    assert_eq!(argv(&plain), "td-mta/deploy");
    let changed = r#"{"error":"production release changed"}"#;
    let other = at_deploy(&job(Some(1), changed, Some(false), Some(true)));
    assert_eq!(other.result, "failed");
    assert!(other.next.is_none());
    let silent = at_deploy(&job(Some(1), "", Some(false), Some(true)));
    assert!(silent.error.unwrap().contains("journal"));
}

#[test]
fn a_stepping_stone_job_ends_naming_the_socket_or_nothing_recover_would_misjudge() {
    let stone = "290c6de7d4f370e85721b970d4d2c17344423aae";
    let (args, opts) = run_of(&["--irreversible", "--revision", stone, "--config", "/t.json"]);
    let opts = opts.unwrap();
    let job = |exit: Option<i32>, fence: Option<bool>, post: Option<bool>| Job {
        receipt: "/var/lib/td-mta-operator/deploy-abcdefgh/receipt.json".to_string(),
        record: td_json::parse(r#"{"status":"verified deployment"}"#).ok(),
        exit,
        fence,
        post,
        stepping: true,
    };
    let at = |j: &Job| ended(&opts, args, j).unwrap();
    let done = at(&job(Some(0), Some(false), Some(true)));
    assert_eq!(done.result, "stepping-stone-deployed");
    assert_eq!(
        done.next.unwrap().join(" "),
        "td-mta/deploy --postgres-socket --config /t.json"
    );
    let checked = at(&job(Some(0), Some(false), Some(false)));
    assert_eq!(checked.result, "check-failed");
    assert!(checked.next.is_none());
    // Lost, fenced or unknown: never --recover, which does not model it.
    for (exit, fence, result) in [
        (None, None, "unknown"),
        (Some(255), Some(false), "unknown"),
        (Some(1), Some(true), "failed"),
        (Some(1), None, "failed"),
    ] {
        let end = at(&job(exit, fence, Some(true)));
        assert_eq!(end.result, result);
        assert!(end.next.is_none());
        assert!(end.error.unwrap().contains("Leaving loopback TLS"));
    }
    // A stone job that failed without its fence ends as any other job.
    let fenceless = at(&job(Some(1), Some(false), Some(true)));
    assert_eq!(fenceless.result, "failed");
    assert!(fenceless.next.is_none());
    assert!(fenceless.error.unwrap().contains("journal"));
    // A socket cutover that left its fence is not --recover's either.
    let (args, opts) = run_of(&["--postgres-socket", "--config", "/t.json"]);
    let opts = opts.unwrap();
    let left = Job {
        stepping: false,
        record: td_json::parse(r#"{"status":"failed","error":"cutover probe failed"}"#).ok(),
        ..job(Some(1), Some(true), Some(true))
    };
    let end = ended(&opts, args, &left).unwrap();
    assert_eq!(end.result, "failed");
    assert!(end.next.is_none() && end.approve.is_none());
    let error = end.error.unwrap();
    assert!(error.starts_with("cutover probe failed; "), "{error}");
    assert!(error.contains("socket_originals"));
    // A fence the cutover found held, an activation's, is --recover's.
    let found = Job {
        record: td_json::parse(
            r#"{"status":"refused","error":"unfinished deployment fence: run td-mta/deploy --recover"}"#,
        )
        .ok(),
        ..left
    };
    let end = ended(&opts, args, &found).unwrap();
    assert_eq!(end.result, "failed");
    assert_eq!(
        end.next.unwrap().join(" "),
        "td-mta/deploy --recover --config /t.json"
    );
}

#[test]
fn a_refusal_before_the_job_carries_its_next_command() {
    let refusal: Box<dyn std::error::Error + Send + Sync> = Box::new(Refusal {
        error: "deploy the stepping-stone release first".to_string(),
        next: Some(vec![
            "td-mta/deploy".to_string(),
            "--irreversible".to_string(),
        ]),
    });
    assert_eq!(
        refusal.to_string(),
        "deploy the stepping-stone release first"
    );
    let refusal = refusal.downcast::<Refusal>().unwrap();
    assert_eq!(refusal.next.unwrap(), ["td-mta/deploy", "--irreversible"]);
}
