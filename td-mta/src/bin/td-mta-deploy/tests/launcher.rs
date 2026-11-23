#![allow(clippy::unwrap_used)]
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
