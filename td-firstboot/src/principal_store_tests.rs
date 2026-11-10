#![allow(clippy::unwrap_used)]
use super::*;
#[path = "../../td-fs/src/temp_dir.rs"]
mod notebook_temp;
use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt;
use std::sync::atomic::{AtomicU64, Ordering};

const SESSION: &str = "td-principals-v1\nsession\t1000\t993\t992\t991\n";
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    paths: Vec<PathBuf>,
    directory: Directory,
}

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "td-principals-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        DirBuilder::new().mode(0o700).create(&path).unwrap();
        let metadata = fs::metadata(&path).unwrap();
        let directory = Directory::open(&path, metadata.uid(), metadata.gid()).unwrap();
        Self {
            paths: vec![path],
            directory,
        }
    }

    fn ledger(&self) -> Registry {
        Registry::parse(&self.directory.read(LEDGER).unwrap().unwrap()).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = fs::remove_dir_all(path);
        }
    }
}

fn app(name: &str, uid: u32) -> Registry {
    Registry::parse(&format!("{SESSION}application\t1000\t{name}\t{uid}\n")).unwrap()
}

#[test]
fn a_launcher_cannot_enroll_missing_or_new_identities_by_checking_them() {
    let fixture = Fixture::new();
    let desired = app("mail", 65537);
    assert!(fixture.directory.verify_enrolled(&desired).is_err());
    assert!(!fixture.directory.path(LEDGER).exists());
    fixture.directory.enroll(&desired).unwrap();
    let before = fs::read(fixture.directory.path(LEDGER)).unwrap();
    assert_eq!(
        fixture.directory.verify_enrolled(&desired).unwrap(),
        desired
    );
    assert!(fixture
        .directory
        .verify_enrolled(&app("news", 65538))
        .is_err());
    assert!(fixture
        .directory
        .verify_enrolled(&app("mail", 65539))
        .is_err());
    let removed = Registry::parse(SESSION).unwrap();
    assert_eq!(
        fixture.directory.verify_enrolled(&removed).unwrap(),
        desired
    );
    assert_eq!(fs::read(fixture.directory.path(LEDGER)).unwrap(), before);
}

#[test]
fn provisioning_is_private_idempotent_and_never_recycles_a_uid() {
    let fixture = Fixture::new();
    let first = app("mail", 65536);
    fixture.directory.enroll(&first).unwrap();
    let before = fs::metadata(fixture.directory.path(LEDGER)).unwrap();
    assert_eq!(before.mode() & 0o7777, 0o600);
    fixture.directory.enroll(&first).unwrap();
    assert_eq!(
        fs::metadata(fixture.directory.path(LEDGER)).unwrap().ino(),
        before.ino()
    );
    fixture
        .directory
        .enroll(&Registry::parse(SESSION).unwrap())
        .unwrap();
    assert_eq!(fixture.ledger(), first);
    assert!(fixture.directory.enroll(&app("news", 65536)).is_err());
    assert!(fixture.directory.enroll(&app("mail", 65537)).is_err());
    assert_eq!(fixture.ledger(), first);
}

#[test]
fn stale_partial_staging_is_removed_before_an_idempotent_retry() {
    let fixture = Fixture::new();
    let first = app("mail", 65536);
    fixture.directory.enroll(&first).unwrap();
    let mut stale = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(fixture.directory.path(STAGED))
        .unwrap();
    stale.write_all(b"td-princi").unwrap();
    fixture.directory.enroll(&first).unwrap();
    assert!(fixture.directory.read(STAGED).unwrap().is_none());
    assert_eq!(fixture.ledger(), first);
}

#[test]
fn symlink_and_hardlink_substitutes_do_not_change_their_targets() {
    use std::os::unix::fs::symlink;
    for name in [LEDGER, STAGED, LOCK] {
        for symbolic in [false, true] {
            let fixture = Fixture::new();
            let target = fixture.directory.path("unrelated");
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&target)
                .unwrap();
            file.write_all(b"unrelated bytes").unwrap();
            if symbolic {
                symlink("unrelated", fixture.directory.path(name)).unwrap();
            } else {
                fs::hard_link(&target, fixture.directory.path(name)).unwrap();
            }
            assert!(fixture.directory.enroll(&app("mail", 65536)).is_err());
            assert_eq!(fs::read(&target).unwrap(), b"unrelated bytes");
        }
    }
}

#[test]
fn corrupt_or_public_ledger_is_not_silently_reinitialized() {
    let fixture = Fixture::new();
    let path = fixture.directory.path(LEDGER);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    file.write_all(b"broken ledger\n").unwrap();
    assert!(fixture.directory.enroll(&app("mail", 65536)).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"broken ledger\n");
    fs::write(&path, SESSION).unwrap();
    file.set_permissions(Permissions::from_mode(0o644)).unwrap();
    assert!(fixture.directory.enroll(&app("mail", 65536)).is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), SESSION);
}

#[test]
fn renamed_directory_stays_pinned_during_publication() {
    let mut fixture = Fixture::new();
    let original = fixture.paths.first().unwrap().clone();
    let moved = original.with_extension("moved");
    fs::rename(&original, &moved).unwrap();
    fixture.paths.push(moved.clone());
    DirBuilder::new().mode(0o700).create(&original).unwrap();
    let registry = app("mail", 65536);
    fixture.directory.enroll(&registry).unwrap();
    assert!(!original.join(LEDGER).exists());
    assert_eq!(
        fs::read_to_string(moved.join(LEDGER)).unwrap(),
        registry.encode()
    );
}

#[test]
fn concurrent_writers_preserve_both_reservations() {
    let fixture = Fixture::new();
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            barrier.wait();
            fixture.directory.enroll(&app("mail", 65536)).unwrap();
        });
        let second = scope.spawn(|| {
            barrier.wait();
            fixture.directory.enroll(&app("news", 65537)).unwrap();
        });
        first.join().unwrap();
        second.join().unwrap();
    });
    assert_eq!(
        fixture.ledger().application(1000, "mail").unwrap().uid,
        65536
    );
    assert_eq!(
        fixture.ledger().application(1000, "news").unwrap().uid,
        65537
    );
}

#[test]
fn account_validation_sees_retired_assignments_and_refusal_cannot_publish() {
    let fixture = Fixture::new();
    let first = app("mail", 65536);
    fixture.directory.enroll(&first).unwrap();
    let result = fixture
        .directory
        .enroll_checked(&app("news", 65537), |retained| {
            assert_eq!(retained.application(1000, "mail").unwrap().uid, 65536);
            assert_eq!(retained.application(1000, "news").unwrap().uid, 65537);
            Err("account collision".into())
        });
    assert!(result.is_err());
    assert_eq!(fixture.ledger(), first);
    assert!(fixture.directory.read(STAGED).unwrap().is_none());
}

#[test]
fn an_existing_lock_serializes_even_when_opened_nonblocking() {
    let fixture = Fixture::new();
    let held = fixture.directory.lock().unwrap();
    let (started, receiving_start) = std::sync::mpsc::channel();
    let (done, receiving_done) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            started.send(()).unwrap();
            done.send(fixture.directory.enroll(&app("mail", 65536)))
                .unwrap();
        });
        receiving_start
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(matches!(
            receiving_done.recv_timeout(std::time::Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        drop(held);
        receiving_done
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
            .unwrap();
    });
}

#[test]
fn interrupted_lock_permissions_recover_but_public_permissions_are_refused() {
    let fixture = Fixture::new();
    let lock = fixture.directory.lock().unwrap();
    lock.set_permissions(Permissions::from_mode(0o000)).unwrap();
    drop(lock);
    fixture.directory.enroll(&app("mail", 65536)).unwrap();
    assert_eq!(
        fs::metadata(fixture.directory.path(LOCK)).unwrap().mode() & 0o7777,
        0o600
    );
    fs::set_permissions(fixture.directory.path(LOCK), Permissions::from_mode(0o644)).unwrap();
    assert!(fixture.directory.enroll(&app("mail", 65536)).is_err());
    assert_eq!(
        fs::metadata(fixture.directory.path(LOCK)).unwrap().mode() & 0o7777,
        0o644
    );
}

#[test]
fn interrupted_staging_with_masked_owner_permissions_is_removed() {
    let fixture = Fixture::new();
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o000)
        .open(fixture.directory.path(STAGED))
        .unwrap();
    drop(file);
    fixture.directory.enroll(&app("mail", 65536)).unwrap();
    assert!(fixture.directory.read(STAGED).unwrap().is_none());
}

#[test]
fn broker_runtimes_repeat_without_following_links_or_accepting_other_writers() {
    let fixture = Fixture::new();
    let parent = &fixture.directory;
    let owner = (parent.uid, parent.gid);
    let first = runtime_child(parent, "bus", owner).unwrap();
    let before = first.file.metadata().unwrap().ino();
    drop(first);
    assert_eq!(
        runtime_child(parent, "bus", owner)
            .unwrap()
            .file
            .metadata()
            .unwrap()
            .ino(),
        before
    );
    fs::set_permissions(parent.path("bus"), Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        runtime_child(parent, "bus", owner)
            .unwrap()
            .file
            .metadata()
            .unwrap()
            .mode()
            & 0o7777,
        0o755
    );
    fs::set_permissions(parent.path("bus"), Permissions::from_mode(0o777)).unwrap();
    assert!(runtime_child(parent, "bus", owner).is_err());
    fs::set_permissions(parent.path("bus"), Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink(parent.path("bus"), parent.path("redirected")).unwrap();
    assert!(runtime_child(parent, "redirected", owner).is_err());
    fs::write(parent.path("file"), b"untouched").unwrap();
    assert!(runtime_child(parent, "file", owner).is_err());
    assert_eq!(fs::read(parent.path("file")).unwrap(), b"untouched");
}

#[test]
fn portal_runtime_repeats_privately_and_refuses_redirects() {
    let fixture = Fixture::new();
    let parent = &fixture.directory;
    let owner = (parent.uid, parent.gid);
    let child = runtime_child_mode(parent, "portal", owner, 0o700).unwrap();
    let inode = child.file.metadata().unwrap().ino();
    assert_eq!(child.file.metadata().unwrap().mode() & 0o7777, 0o700);
    let again = runtime_child_mode(parent, "portal", owner, 0o700).unwrap();
    assert_eq!(again.file.metadata().unwrap().ino(), inode);
    std::os::unix::fs::symlink(parent.path("portal"), parent.path("redirected")).unwrap();
    assert!(runtime_child_mode(parent, "redirected", owner, 0o700).is_err());
    fs::set_permissions(parent.path("portal"), Permissions::from_mode(0o777)).unwrap();
    assert!(runtime_child_mode(parent, "portal", owner, 0o700).is_err());
}

#[test]
fn notebook_ledger_preserves_legacy_boots_and_recovers_partial_publication() {
    let temp = notebook_temp::TempDir::new("td-notebook-ledger").unwrap();
    let metadata = fs::metadata(temp.path()).unwrap();
    let directory = Directory::open(temp.path(), metadata.uid(), metadata.gid()).unwrap();
    let original = app("mail", 65537);
    directory.enroll(&original).unwrap();
    let desired = Registry::parse(
        &original
            .encode()
            .replace("application", "notebook\t1000\t989\napplication"),
    )
    .unwrap();
    directory.enroll(&desired).unwrap();
    assert_eq!(directory.read(LEDGER).unwrap().unwrap(), original.encode());
    assert_eq!(directory.verify_enrolled(&desired).unwrap(), desired);
    // A previous deployment may update its own known ledger. New boots
    // merge it without discarding either new or older reservations.
    let older = app("news", 65538).enroll(&original).unwrap();
    directory.publish(LEDGER, STAGED, &older.encode()).unwrap();
    let combined = desired.enroll(&older).unwrap();
    assert_eq!(directory.enroll(&original).unwrap(), combined);
    // Crash after the full record's rename, before the legacy rename.
    fs::remove_file(directory.path(LEDGER)).unwrap();
    assert_eq!(directory.enroll(&desired).unwrap(), combined);
    assert_eq!(
        directory.read(LEDGER).unwrap().unwrap(),
        combined.legacy().encode()
    );
    let reused = Registry::parse(&original.encode().replace("\t991\n", "\t989\n")).unwrap();
    assert!(directory.enroll(&reused).is_err());
    temp.cleanup().unwrap();
}

const SEEDED: &str = "td-accounts-v1\napplication\tfirefox\t65536\napplication\tmail\t65537\napplication\tnews\t65538\napplication\tweb\t65540\n";

fn account_directory(temp: &notebook_temp::TempDir) -> Directory {
    let metadata = fs::metadata(temp.path()).unwrap();
    Directory::open(temp.path(), metadata.uid(), metadata.gid()).unwrap()
}

fn account_ledger(directory: &Directory) -> Option<String> {
    directory
        .read_as(workload_accounts::FILE_NAME, ACCOUNTS_MODE, ACCOUNTS_LIMIT)
        .unwrap()
}

fn public_file(path: &Path, text: &str, mode: u32) {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .unwrap();
    file.write_all(text.as_bytes()).unwrap();
    file.set_permissions(Permissions::from_mode(mode)).unwrap();
}

#[test]
fn boot_enrollment_seeds_a_public_account_ledger_idempotently() {
    let temp = notebook_temp::TempDir::new("td-account-ledger").unwrap();
    let directory = account_directory(&temp);
    let desired = app("mail", 65537);
    let ledger = directory.enroll_checked(&desired, |_| Ok(())).unwrap();
    assert_eq!(ledger.encode(), SEEDED);
    assert_eq!(account_ledger(&directory).as_deref(), Some(SEEDED));
    let path = directory.path(workload_accounts::FILE_NAME);
    let before = fs::metadata(&path).unwrap();
    assert_eq!(before.mode() & 0o7777, 0o644);
    assert_eq!(
        directory.enroll_checked(&desired, |_| Ok(())).unwrap(),
        ledger
    );
    assert_eq!(fs::metadata(&path).unwrap().ino(), before.ino());
    // The reservations are published beside it, private as before.
    assert_eq!(directory.verify_enrolled(&desired).unwrap(), desired);
    assert!(!directory.path(ACCOUNTS_STAGED).exists());
    temp.cleanup().unwrap();
}

#[test]
fn a_removed_applications_reservation_keeps_its_account_row() {
    let temp = notebook_temp::TempDir::new("td-account-ledger").unwrap();
    let directory = account_directory(&temp);
    // A boot before the ledger, enrolled while Claude Code shipped at 65539.
    let retired = Registry::parse(&format!(
        "{SESSION}application\t1000\tclaude\t65539\napplication\t1000\tmail\t65537\n"
    ))
    .unwrap();
    directory.enroll(&retired).unwrap();
    assert!(account_ledger(&directory).is_none());
    let ledger = directory
        .enroll_checked(&app("mail", 65537), |_| Ok(()))
        .unwrap();
    assert_eq!(
        ledger.encode(),
        SEEDED.replace(
            "application\tfirefox",
            "application\tclaude\t65539\napplication\tfirefox"
        )
    );
    assert_eq!(account_ledger(&directory), Some(ledger.encode()));
    // Rows beyond the reservations, as an allocation and a later
    // deployment's workspace add, are kept.
    let allocated = format!(
        "{}workspace\tagent-1\t98304\n",
        ledger.encode().replace(
            "application\tweb",
            "application\tphoto\t65541\napplication\tweb"
        )
    );
    directory
        .publish_as(
            workload_accounts::FILE_NAME,
            ACCOUNTS_STAGED,
            &allocated,
            ACCOUNTS_MODE,
            ACCOUNTS_LIMIT,
        )
        .unwrap();
    assert_eq!(
        directory
            .enroll_checked(&app("mail", 65537), |_| Ok(()))
            .unwrap()
            .encode(),
        allocated
    );
    assert_eq!(account_ledger(&directory), Some(allocated));
    temp.cleanup().unwrap();
}

/// Neither reservation file nor the account ledger exists, or each keeps
/// `before`'s bytes.
fn unchanged(directory: &Directory, before: &[(&str, Option<Vec<u8>>)]) {
    for (name, bytes) in before {
        assert_eq!(&fs::read(directory.path(name)).ok(), bytes, "{name}");
    }
}

fn snapshot(directory: &Directory) -> Vec<(&'static str, Option<Vec<u8>>)> {
    [LEDGER, NOTEBOOK_LEDGER, workload_accounts::FILE_NAME]
        .into_iter()
        .map(|name| (name, fs::read(directory.path(name)).ok()))
        .collect()
}

#[test]
fn a_refused_seed_publishes_no_reservation() {
    let notebook =
        |rows: &str| Registry::parse(&format!("{SESSION}notebook\t1000\t989\n{rows}")).unwrap();
    let refusals = [
        // mail at the uid the ledger carries for firefox.
        notebook("application\t1000\tmail\t65536\n"),
        // A uid the principal table admits and the ledger does not.
        notebook("application\t1000\tphoto\t98304\n"),
        // One name under two owners: the ledger keys rows by name alone.
        Registry::parse(
            "td-principals-v1\nsession\t1000\t993\t992\t991\nsession\t1001\t990\t988\t987\napplication\t1000\tmail\t65537\napplication\t1001\tmail\t65541\n",
        )
        .unwrap(),
    ];
    for desired in &refusals {
        // On a first boot nothing is published.
        let temp = notebook_temp::TempDir::new("td-account-ledger").unwrap();
        let directory = account_directory(&temp);
        assert!(directory.enroll_checked(desired, |_| Ok(())).is_err());
        unchanged(&directory, &[(LEDGER, None), (NOTEBOOK_LEDGER, None)]);
        assert!(account_ledger(&directory).is_none());
        assert!(check_accounts(desired).is_err());
        temp.cleanup().unwrap();
        // After an enrolled boot every file keeps its bytes.
        let temp = notebook_temp::TempDir::new("td-account-ledger").unwrap();
        let directory = account_directory(&temp);
        directory
            .enroll_checked(&notebook("application\t1000\tnews\t65538\n"), |_| Ok(()))
            .unwrap();
        let before = snapshot(&directory);
        assert!(before.iter().all(|(_, bytes)| bytes.is_some()));
        assert!(directory.enroll_checked(desired, |_| Ok(())).is_err());
        unchanged(&directory, &before);
        temp.cleanup().unwrap();
    }
    check_accounts(&notebook(
        "application\t1000\tclaude\t65539\napplication\t1000\tmail\t65537\n",
    ))
    .unwrap();
}

#[test]
fn a_reassignment_or_malformed_account_ledger_refuses_and_is_never_reset() {
    for text in [
        "broken ledger\n".to_string(),
        SEEDED.replace("mail\t65537", "mail\t65541"),
        SEEDED.replace("application\tnews\t65538\n", "application\tnews\t65541\n"),
    ] {
        let temp = notebook_temp::TempDir::new("td-account-ledger").unwrap();
        let directory = account_directory(&temp);
        let path = directory.path(workload_accounts::FILE_NAME);
        public_file(&path, &text, 0o644);
        assert!(
            directory
                .enroll_checked(&app("mail", 65537), |_| Ok(()))
                .is_err(),
            "{text:?}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        unchanged(&directory, &[(LEDGER, None), (NOTEBOOK_LEDGER, None)]);
        temp.cleanup().unwrap();
    }
}

#[test]
fn an_account_ledger_of_another_mode_owner_link_or_size_is_refused() {
    use std::os::unix::fs::symlink;
    let oversized = format!("{SEEDED}{}", "x".repeat(workload_accounts::MAX_BYTES));
    for case in [
        "private",
        "group-writable",
        "symlink",
        "hardlink",
        "oversized",
    ] {
        let temp = notebook_temp::TempDir::new("td-account-ledger").unwrap();
        let directory = account_directory(&temp);
        let path = directory.path(workload_accounts::FILE_NAME);
        let target = directory.path("unrelated");
        let (text, mode) = match case {
            "private" => (SEEDED, 0o600),
            "group-writable" => (SEEDED, 0o664),
            "oversized" => (oversized.as_str(), 0o644),
            _ => (SEEDED, 0o644),
        };
        public_file(&target, text, mode);
        match case {
            "symlink" => symlink("unrelated", &path).unwrap(),
            "hardlink" => fs::hard_link(&target, &path).unwrap(),
            _ => fs::rename(&target, &path).unwrap(),
        }
        let before = fs::read(&path).unwrap();
        assert!(
            directory
                .enroll_checked(&app("mail", 65537), |_| Ok(()))
                .is_err(),
            "{case}"
        );
        assert_eq!(fs::read(&path).unwrap(), before, "{case}");
        temp.cleanup().unwrap();
    }
    // A directory pinned for another owner refuses this owner's ledger, as
    // `/var/lib/td`'s refuses any but root's.
    let temp = notebook_temp::TempDir::new("td-account-ledger").unwrap();
    let directory = account_directory(&temp);
    directory
        .enroll_checked(&app("mail", 65537), |_| Ok(()))
        .unwrap();
    for (uid, gid) in [
        (directory.uid.wrapping_add(1), directory.gid),
        (directory.uid, directory.gid.wrapping_add(1)),
    ] {
        let foreign = Directory {
            file: directory.file.try_clone().unwrap(),
            uid,
            gid,
        };
        assert!(foreign
            .read_as(workload_accounts::FILE_NAME, ACCOUNTS_MODE, ACCOUNTS_LIMIT)
            .is_err());
    }
    assert_eq!(account_ledger(&directory).as_deref(), Some(SEEDED));
    temp.cleanup().unwrap();
}

#[test]
fn interrupted_account_staging_is_removed_but_other_writers_are_refused() {
    let temp = notebook_temp::TempDir::new("td-account-ledger").unwrap();
    let directory = account_directory(&temp);
    let staged = directory.path(ACCOUNTS_STAGED);
    // A crash mid-write, then one before the mode was set.
    for mode in [0o644, 0o000] {
        public_file(&staged, "td-acc", mode);
        directory
            .enroll_checked(&app("mail", 65537), |_| Ok(()))
            .unwrap();
        assert!(!staged.exists());
        assert_eq!(account_ledger(&directory).as_deref(), Some(SEEDED));
    }
    public_file(&staged, "td-acc", 0o666);
    assert!(directory
        .enroll_checked(&app("mail", 65537), |_| Ok(()))
        .is_err());
    assert!(staged.exists());
    temp.cleanup().unwrap();
}
