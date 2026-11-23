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
