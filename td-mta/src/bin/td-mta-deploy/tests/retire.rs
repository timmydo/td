#![allow(clippy::unwrap_used)]
use super::*;

#[test]
fn material_goes_only_after_its_seven_days() {
    let day = 24 * 3600;
    let material = [
        Material {
            path: PathBuf::from("/old"),
            since: 1_000_000,
        },
        Material {
            path: PathBuf::from("/young"),
            since: 1_000_000 + 5 * day,
        },
        Material {
            path: PathBuf::from("/younger"),
            since: 1_000_000 + 6 * day,
        },
    ];
    let (past, next) = due(&material, 1_000_000 + 7 * day).unwrap();
    assert_eq!(past, [Path::new("/old")]);
    assert_eq!(next, Some(1_000_000 + 12 * day));
    let (past, next) = due(&material, 1_000_000 + 7 * day - 1).unwrap();
    assert!(past.is_empty());
    assert_eq!(next, Some(1_000_000 + 7 * day));
    let (past, next) = due(&material, 1_000_000 + 20 * day).unwrap();
    assert_eq!(past.len(), 3);
    assert_eq!(next, None);
    let (past, next) = due(&[], 0).unwrap();
    assert!(past.is_empty() && next.is_none());
    assert_eq!(WINDOW, 7 * day);
}

#[test]
fn only_job_and_recovery_directories_but_the_current_one_are_material() {
    let dir = td_fs::TempDir::new("retire-material").unwrap();
    let root = dir.path();
    for name in [
        "deploy-a1b2c3d4",
        "deploy-0123456789abcdef01234567",
        "deploy-current1",
        "deploy-short",
        "host.json",
        "physical-backup",
        ".retired-deploy-e5f6a7b8",
        ".retired-other",
    ] {
        fs::create_dir(root.join(name)).unwrap();
    }
    fs::write(root.join("deploy-a1b2c3d4/receipt.json"), "{}").unwrap();
    let (material, tombstones) = material_in(root, &root.join("deploy-current1")).unwrap();
    let mut names: Vec<_> = material
        .iter()
        .map(|m| m.path.file_name().unwrap().to_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["deploy-0123456789abcdef01234567", "deploy-a1b2c3d4"]
    );
    assert_eq!(tombstones, [root.join(".retired-deploy-e5f6a7b8")]);
    let (none, dead) = material_in(&root.join("absent"), root).unwrap();
    assert!(none.is_empty() && dead.is_empty());
    assert!(material_name("deploy-a1b2c3d4"));
    assert!(!material_name("deploy-a1b2c3d"));
    assert!(!material_name("deploy-a1b2c3d4/"));
    assert_eq!(
        tombstone(&root.join("deploy-a1b2c3d4")).unwrap(),
        root.join(".retired-deploy-a1b2c3d4")
    );
    assert_eq!(
        tombstone(&root.join(".retired-deploy-e5f6a7b8")).unwrap(),
        root.join(".retired-deploy-e5f6a7b8")
    );
}

#[test]
fn material_is_removed_only_as_an_owned_directory_without_mounts() {
    let dir = td_fs::TempDir::new("retire-removable").unwrap();
    let root = dir.path();
    let job = root.join("deploy-a1b2c3d4");
    fs::create_dir(&job).unwrap();
    let me = fs::metadata(root).unwrap().uid();
    assert!(removable(&job, "", me).is_ok());
    assert!(removable(&job, "", me + 1).is_err());
    let link = root.join("deploy-linklink");
    std::os::unix::fs::symlink(&job, &link).unwrap();
    assert!(removable(&link, "", me).is_err());
    fs::write(root.join("deploy-filefile"), "").unwrap();
    assert!(removable(&root.join("deploy-filefile"), "", me).is_err());
    let path = job.to_str().unwrap();
    let inside = format!("36 25 0:32 / {path}/mnt rw - tmpfs tmpfs rw\n");
    assert!(removable(&job, &inside, me).is_err());
    let at = format!("36 25 0:32 / {path} rw - tmpfs tmpfs rw\n");
    assert!(removable(&job, &at, me).is_err());
    let beside = format!("36 25 0:32 / {path}x rw - tmpfs tmpfs rw\n");
    assert!(removable(&job, &beside, me).is_ok());
}

#[test]
fn material_goes_under_its_tombstone_after_it_is_announced() {
    let dir = td_fs::TempDir::new("retire-remove").unwrap();
    let root = dir.path();
    let job = root.join("deploy-a1b2c3d4");
    fs::create_dir_all(job.join("original/base")).unwrap();
    fs::write(job.join("original/base/1"), "x").unwrap();
    // A refused announcement removes nothing.
    assert!(remove(&job, |_| Err("receipt".into())).is_err());
    assert!(job.join("original/base/1").exists());
    let mut announced = None;
    remove(&job, |path| {
        announced = Some(path.to_path_buf());
        assert!(path.exists());
        Ok(())
    })
    .unwrap();
    assert_eq!(announced.as_deref(), Some(job.as_path()));
    assert!(!job.exists() && !root.join(".retired-deploy-a1b2c3d4").exists());
    // A run that died after the rename: a rerun finishes the tombstone.
    let dead = root.join(".retired-deploy-e5f6a7b8");
    fs::create_dir_all(dead.join("x")).unwrap();
    remove(&dead, |_| Ok(())).unwrap();
    assert!(!dead.exists());
    assert_eq!(fs::read_dir(root).unwrap().count(), 0);
}

#[test]
fn the_debian_tree_counts_from_the_switch_that_left_it() {
    let dir = td_fs::TempDir::new("retire-debian").unwrap();
    let root = dir.path();
    let record = root.join("since");
    let tree = root.join("postgresql");
    assert_eq!(switched_at(&record), None);
    record_switch(&record, 100, false).unwrap();
    assert_eq!(switched_at(&record), Some(100));
    // A verified rerun keeps a recorded window; a verified switch or a
    // rollback's put-back writes its own.
    record_switch(&record, 200, false).unwrap();
    assert_eq!(switched_at(&record), Some(100));
    record_switch(&record, 300, true).unwrap();
    assert_eq!(switched_at(&record), Some(300));
    // Unreadable in any way counts as absent.
    fs::write(&record, "garbage").unwrap();
    assert_eq!(switched_at(&record), None);
    fs::write(&record, "1".repeat(100)).unwrap();
    assert_eq!(switched_at(&record), None);
    record_switch(&record, 400, false).unwrap();
    assert_eq!(switched_at(&record), Some(400));
    // A staged file a crash left is replaced, never kept.
    fs::write(root.join(".since.new"), "9").unwrap();
    record_switch(&record, 500, true).unwrap();
    assert_eq!(switched_at(&record), Some(500));
    clear_switch(&record).unwrap();
    clear_switch(&record).unwrap();
    assert_eq!(switched_at(&record), None);
    assert_eq!(fs::read_dir(root).unwrap().count(), 0);
    // A failed write keeps the old value and leaves no staged file.
    record_switch(&record, 600, true).unwrap();
    fs::create_dir(root.join(".since.new")).unwrap();
    assert!(record_switch(&record, 700, true).is_err());
    assert_eq!(switched_at(&record), Some(600));
    fs::remove_dir(root.join(".since.new")).unwrap();
    clear_switch(&record).unwrap();

    // No tree, or the Debian server selected: not material.
    assert!(matches!(
        debian_material(&tree, &record, true).unwrap(),
        (None, None)
    ));
    fs::create_dir(&tree).unwrap();
    assert!(matches!(
        debian_material(&tree, &record, false).unwrap(),
        (None, None)
    ));
    // On td's server without a verified switch recorded: it stays, and
    // nothing is recorded for it.
    assert!(matches!(
        debian_material(&tree, &record, true).unwrap(),
        (None, None)
    ));
    assert!(!record.exists());
    record_switch(&record, 5, true).unwrap();
    let (found, dead) = debian_material(&tree, &record, true).unwrap();
    let found = found.unwrap();
    assert_eq!((found.path.as_path(), found.since), (tree.as_path(), 5));
    assert!(dead.is_none());
    // A tombstone beside a live tree is finished whatever is selected.
    fs::create_dir(root.join(".retired-postgresql")).unwrap();
    let (_, dead) = debian_material(&tree, &record, false).unwrap();
    assert_eq!(dead, Some(root.join(".retired-postgresql")));
    let (found, dead) = debian_material(&tree, &record, true).unwrap();
    assert!(found.is_some() && dead.is_some());
}

#[test]
fn a_switch_takes_the_window_and_only_verification_or_rollback_writes_it() {
    let dir = td_fs::TempDir::new("retire-window").unwrap();
    let root = dir.path();
    let record = root.join("since");
    let tree = root.join("postgresql");
    fs::create_dir(&tree).unwrap();
    record_switch(&record, 100, true).unwrap();
    // Before the stop: the time so far, and nothing left meanwhile.
    assert_eq!(take_switch(&record).unwrap(), Some(100));
    assert!(!record.exists());
    assert_eq!(take_switch(&record).unwrap(), None);
    fs::write(&record, "garbage").unwrap();
    assert_eq!(take_switch(&record).unwrap(), None);
    assert!(!record.exists());
    // A rollback to a td source puts back that closure's own time.
    assert!(put_back(&record, &tree, true, Some(100)).unwrap());
    assert_eq!(switched_at(&record), Some(100));
    clear_switch(&record).unwrap();
    assert!(!put_back(&record, &tree, false, Some(100)).unwrap());
    assert!(!put_back(&record, &tree, true, None).unwrap());
    assert!(!record.exists());
    // A verified rerun after a taken window starts it now.
    start_window(&record, &tree, 500, false).unwrap();
    assert_eq!(switched_at(&record), Some(500));
    start_window(&record, &tree, 600, false).unwrap();
    assert_eq!(switched_at(&record), Some(500));
    start_window(&record, &tree, 700, true).unwrap();
    assert_eq!(switched_at(&record), Some(700));
    // Once the tree is gone nothing is written.
    clear_switch(&record).unwrap();
    fs::remove_dir(&tree).unwrap();
    start_window(&record, &tree, 800, true).unwrap();
    assert!(!put_back(&record, &tree, true, Some(100)).unwrap());
    assert!(!record.exists());
}

#[test]
fn retirement_needs_the_flag_no_fence_and_the_live_release() {
    let named = td_json::parse(r#"{"irreversible":true}"#).unwrap();
    let is = |v: bool| move || -> R<bool> { Ok(v) };
    let broken = || -> R<bool> { Err("ENOENT".into()) };
    let refusal = |r: R<()>| r.unwrap_err().to_string();
    preflight(&named, is(false), is(true)).unwrap();
    for job in [
        r#"{"irreversible":false}"#,
        "{}",
        r#"{"irreversible":"yes"}"#,
    ] {
        let refused = refusal(preflight(
            &td_json::parse(job).unwrap(),
            is(false),
            is(true),
        ));
        assert!(refused.contains("--irreversible"), "{refused}");
    }
    let fenced = refusal(preflight(&named, is(true), is(true)));
    assert!(fenced.contains("--recover"), "{fenced}");
    let moved = refusal(preflight(&named, is(false), is(false)));
    assert!(moved.contains("keeps the live release"), "{moved}");
    // In order, each check before what the next one reads: the flag
    // before the fence, the fence before the live release.
    let unflagged = td_json::parse("{}").unwrap();
    assert!(refusal(preflight(&unflagged, broken, broken)).contains("cannot be undone"));
    assert!(refusal(preflight(&named, is(true), broken)).contains("--recover"));
    assert_eq!(refusal(preflight(&named, is(false), broken)), "ENOENT");
}

#[test]
fn the_debian_tree_stays_while_anything_could_still_use_it() {
    // On the Debian server the tree is what runs: no reason recorded.
    assert_eq!(tree_kept(false, &Ok(()), true).unwrap(), (false, None));
    // td's server selected and running alone with a recorded window.
    assert_eq!(tree_kept(true, &Ok(()), true).unwrap(), (true, None));
    // Not what runs: kept, naming the switch to run.
    let stray: R<()> = Err("the running server maps the Debian tree".into());
    let (selected, why) = tree_kept(true, &stray, true).unwrap();
    assert!(!selected);
    let why = why.unwrap();
    assert!(why.starts_with("the running server maps the Debian tree"));
    assert!(why.ends_with("--postgres-binaries"));
    // No verified switch recorded its window.
    let (selected, why) = tree_kept(true, &Ok(()), false).unwrap();
    assert!(selected);
    assert!(why.unwrap().contains("no verified switch"));
}
