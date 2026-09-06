#![allow(clippy::unwrap_used)]
use super::*;
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
