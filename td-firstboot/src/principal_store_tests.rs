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

const GRANTS_SEED: &str = "td-grants-v1\napplication\tfirefox\tdirectory\trw\tDownloads\n";

fn grant_table(directory: &Directory) -> Option<String> {
    directory
        .read_as(workload_grants::FILE_NAME, GRANTS_MODE, GRANTS_LIMIT)
        .unwrap()
}

/// The first boot seeds Firefox's `Downloads` grant, private to root, and
/// no later boot rewrites the table.
#[test]
fn boot_enrollment_seeds_a_private_grant_table_once() {
    let temp = notebook_temp::TempDir::new("td-grant-table").unwrap();
    let directory = account_directory(&temp);
    directory
        .enroll_checked(&app("mail", 65537), |_| Ok(()))
        .unwrap();
    assert_eq!(grant_table(&directory).as_deref(), Some(GRANTS_SEED));
    let path = directory.path(workload_grants::FILE_NAME);
    let before = fs::metadata(&path).unwrap();
    assert_eq!(before.mode() & 0o7777, 0o600);
    assert_eq!(before.nlink(), 1);
    directory
        .enroll_checked(&app("mail", 65537), |_| Ok(()))
        .unwrap();
    let after = fs::metadata(&path).unwrap();
    assert_eq!(
        (after.ino(), after.mtime(), after.mtime_nsec()),
        (before.ino(), before.mtime(), before.mtime_nsec())
    );
    assert!(!directory.path(GRANTS_STAGED).exists());
    temp.cleanup().unwrap();
}

/// A present table is only validated: a revoked seed row is not seeded
/// again, and rows a later deployment wrote are kept.
#[test]
fn a_present_grant_table_is_never_reseeded_or_rewritten() {
    for text in [
        "td-grants-v1\n",
        "td-grants-v1\napplication\tfirefox\tdirectory\tro\tDocuments\n",
        "td-grants-v1\napplication\tmail\tdirectory\trw\tDownloads\nsandbox\tdefault\tnetwork\toff\nworkspace\tagent-1\tsocket\twayland\n",
    ] {
        let temp = notebook_temp::TempDir::new("td-grant-table").unwrap();
        let directory = account_directory(&temp);
        public_file(&directory.path(workload_grants::FILE_NAME), text, 0o600);
        directory
            .enroll_checked(&app("mail", 65537), |_| Ok(()))
            .unwrap();
        assert_eq!(grant_table(&directory).as_deref(), Some(text));
        // The rest of the boot's enrollment went ahead.
        assert_eq!(account_ledger(&directory).as_deref(), Some(SEEDED));
        temp.cleanup().unwrap();
    }
}

/// A malformed table, or one of another mode, owner, link or size, refuses
/// the boot, publishes nothing and is never reset.
#[test]
fn a_malformed_or_misshapen_grant_table_refuses_and_is_kept() {
    use std::os::unix::fs::symlink;
    let oversized = format!("{GRANTS_SEED}{}", "x".repeat(workload_grants::MAX_BYTES));
    for case in [
        "malformed",
        "overlapping",
        "reserved",
        "public",
        "group-readable",
        "symlink",
        "hardlink",
        "oversized",
    ] {
        let temp = notebook_temp::TempDir::new("td-grant-table").unwrap();
        let directory = account_directory(&temp);
        let path = directory.path(workload_grants::FILE_NAME);
        let target = directory.path("unrelated");
        let overlapping = "td-grants-v1\napplication\tfirefox\tdirectory\tro\tDownloads\napplication\tfirefox\tdirectory\trw\tDownloads/a\n";
        let (text, mode) = match case {
            "malformed" => ("td-grants-v1\nbroken\n", 0o600),
            "overlapping" => (overlapping, 0o600),
            "reserved" => (
                "td-grants-v1\napplication\tfirefox\tdirectory\trw\t.ssh\n",
                0o600,
            ),
            "public" => (GRANTS_SEED, 0o644),
            "group-readable" => (GRANTS_SEED, 0o640),
            "oversized" => (oversized.as_str(), 0o600),
            _ => (GRANTS_SEED, 0o600),
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
        unchanged(
            &directory,
            &[
                (LEDGER, None),
                (NOTEBOOK_LEDGER, None),
                (workload_accounts::FILE_NAME, None),
            ],
        );
        temp.cleanup().unwrap();
    }
    // A directory pinned for another owner refuses this owner's table, as
    // `/var/lib/td`'s refuses any but root's.
    let temp = notebook_temp::TempDir::new("td-grant-table").unwrap();
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
        assert!(foreign.seeded_grants().is_err());
    }
    temp.cleanup().unwrap();
}

#[test]
fn interrupted_grant_staging_is_removed_but_other_writers_are_refused() {
    let temp = notebook_temp::TempDir::new("td-grant-table").unwrap();
    let directory = account_directory(&temp);
    let staged = directory.path(GRANTS_STAGED);
    for mode in [0o600, 0o000] {
        public_file(&staged, "td-gra", mode);
        directory
            .enroll_checked(&app("mail", 65537), |_| Ok(()))
            .unwrap();
        assert!(!staged.exists());
        assert_eq!(grant_table(&directory).as_deref(), Some(GRANTS_SEED));
        fs::remove_file(directory.path(workload_grants::FILE_NAME)).unwrap();
    }
    public_file(&staged, "td-gra", 0o640);
    assert!(directory
        .enroll_checked(&app("mail", 65537), |_| Ok(()))
        .is_err());
    assert!(staged.exists());
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
    [
        LEDGER,
        NOTEBOOK_LEDGER,
        workload_accounts::FILE_NAME,
        workload_grants::FILE_NAME,
    ]
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
        assert!(check_accounts(desired, None).is_err());
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
}

/// The image's principal table reserves no application, and every
/// application the image declares has a carried ledger row.
#[test]
fn the_image_check_wants_declarations_the_carried_rows_cover() {
    use crate::app_policy::Declarations;
    let image = Registry::parse(&format!("{SESSION}notebook\t1000\t989\n")).unwrap();
    let declared =
        |rows: &str| Declarations::parse(&format!("td-bus-applications-v2\t1000\n{rows}")).unwrap();
    let carried = declared(
        "firefox\tservice\torg.mozilla.firefox\nmail\tpersonal\t\nnews\tservice\t\nweb\tservice\t\n",
    );
    check_accounts(&image, None).unwrap();
    check_accounts(&image, Some(&carried)).unwrap();
    // claude's row is imported from a retained reservation, never carried.
    for uncarried in ["claude\tservice\t\n", "photo\tpersonal\t\n"] {
        assert!(check_accounts(&image, Some(&declared(uncarried))).is_err());
    }
    // A Flathub declaration needs no carried row: an install allocates it.
    // Firefox is the only real one, and carried, so a made-up second one.
    let photo = declared("firefox\tservice\torg.mozilla.firefox\nphoto\tservice\t\n");
    let flathub: fn(&str) -> bool = |name| ["firefox", "photo"].contains(&name);
    check_accounts_with(&image, Some(&photo), flathub).unwrap();
    assert!(check_accounts(&image, Some(&photo)).is_err());
    // An application the image ships still needs one.
    let shipped = declared("photo\tservice\t\nwriter\tservice\t\n");
    assert!(check_accounts_with(&image, Some(&shipped), flathub).is_err());
    // An application row in the image's own table refuses, even one the
    // ledger carries at that uid.
    let reserving = Registry::parse(&format!(
        "{SESSION}notebook\t1000\t989\napplication\t1000\tmail\t65537\n"
    ))
    .unwrap();
    assert!(check_accounts(&reserving, None).is_err());
    assert!(check_accounts(&reserving, Some(&carried)).is_err());
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

/// The launcher's uid is the ledger's account for a service application
/// and the session owner for a personal one, and a retained reservation
/// that disagrees with the ledger refuses.
#[test]
fn the_launch_uid_comes_from_the_ledger() {
    use crate::app_policy::Policy;
    const DECLARED: &str = "td-bus-applications-v2\t1000\nfirefox\tservice\torg.mozilla.firefox\nmail\tpersonal\t\nnews\tservice\t\n";
    const LEDGER: &str = "td-accounts-v1\napplication\tfirefox\t65536\napplication\tmail\t65537\n";
    let policy = Policy::from_tables(DECLARED, LEDGER).unwrap();
    let none = Registry::parse(SESSION).unwrap();
    assert_eq!(launch_uid(&none, &policy, 1000, "firefox").unwrap(), 65536);
    assert_eq!(launch_uid(&none, &policy, 1000, "mail").unwrap(), 1000);
    // Declared without a ledger row, undeclared, or another session's.
    assert!(launch_uid(&none, &policy, 1000, "news").is_err());
    assert!(launch_uid(&none, &policy, 1000, "web").is_err());
    assert!(launch_uid(&none, &policy, 1001, "firefox").is_err());
    // A retained reservation at the ledger's uid agrees; at another refuses.
    assert_eq!(
        launch_uid(&app("firefox", 65536), &policy, 1000, "firefox").unwrap(),
        65536
    );
    assert!(launch_uid(&app("firefox", 65541), &policy, 1000, "firefox").is_err());
    assert!(launch_uid(&app("mail", 65541), &policy, 1000, "mail").is_err());
}

/// A fixture whose principal ledger reserves `registry` and whose account
/// ledger is `accounts`, published at its public mode.
fn allocation_fixture(
    registry: &Registry,
    accounts: Option<&str>,
) -> (notebook_temp::TempDir, Directory) {
    let temp = notebook_temp::TempDir::new("td-account-allocation").unwrap();
    let directory = account_directory(&temp);
    directory.enroll(registry).unwrap();
    if let Some(accounts) = accounts {
        directory
            .publish_as(
                workload_accounts::FILE_NAME,
                ACCOUNTS_STAGED,
                accounts,
                ACCOUNTS_MODE,
                ACCOUNTS_LIMIT,
            )
            .unwrap();
    }
    (temp, directory)
}

fn no_ids() -> Result<BTreeSet<u32>, String> {
    Ok(BTreeSet::new())
}

/// The session owner's declarations the allocation tests install against.
fn installable() -> crate::app_policy::Declarations {
    crate::app_policy::Declarations::parse(
        "td-bus-applications-v2\t1000\nfirefox\tservice\torg.mozilla.firefox\nmail\tpersonal\t\nnotes\tservice\t\nphoto\tservice\t\nweb\tservice\t\nwriter\tservice\t\n",
    )
    .unwrap()
}

/// `directory`'s allocation of `name` for the session owner, recording each
/// preparation's uid and whether its row was new.
fn allocate_into(
    directory: &Directory,
    name: &str,
    prepared: &mut Vec<(u32, bool)>,
) -> Result<u32, String> {
    directory.allocate(1000, name, &installable(), no_ids, |uid, fresh| {
        prepared.push((uid, fresh));
        Ok(())
    })
}

/// An install's allocation publishes the new row at the ledger's public
/// mode, then prepares the account as new; a second allocation, a
/// reinstall's, returns the same uid, publishes nothing and prepares again,
/// not as new.
#[test]
fn an_allocation_publishes_one_public_row_and_repeats_idempotently() {
    let (temp, directory) = allocation_fixture(&Registry::parse(SESSION).unwrap(), Some(SEEDED));
    let mut prepared = Vec::new();
    let uid = allocate_into(&directory, "photo", &mut prepared).unwrap();
    // Past the carried block, though Claude's retired 65539 is free.
    assert_eq!(uid, 66560);
    let path = directory.path(workload_accounts::FILE_NAME);
    let published = fs::metadata(&path).unwrap();
    assert_eq!(published.mode() & 0o7777, 0o644);
    assert_eq!(
        account_ledger(&directory).unwrap(),
        SEEDED.replace(
            "application\tweb",
            "application\tphoto\t66560\napplication\tweb"
        )
    );
    assert!(!directory.path(ACCOUNTS_STAGED).exists());
    assert_eq!(
        allocate_into(&directory, "photo", &mut prepared).unwrap(),
        66560
    );
    assert_eq!(prepared, [(66560, true), (66560, false)]);
    assert_eq!(fs::metadata(&path).unwrap().ino(), published.ino());
    // A carried row is returned as it stands.
    assert_eq!(
        allocate_into(&directory, "firefox", &mut prepared).unwrap(),
        65536
    );
    assert_eq!(fs::metadata(&path).unwrap().ino(), published.ino());
    // A preparation killed or failed keeps the row; the next install
    // retries it, not as new, so it completes what root left.
    assert!(directory
        .allocate(1000, "writer", &installable(), no_ids, |_, _| Err(
            "no cgroup".into()
        ))
        .is_err());
    let ledger = Ledger::parse(&account_ledger(&directory).unwrap()).unwrap();
    assert_eq!(ledger.uid_of(Kind::Application, "writer"), Some(66561));
    prepared.clear();
    assert_eq!(
        allocate_into(&directory, "writer", &mut prepared).unwrap(),
        66561
    );
    assert_eq!(prepared, [(66561, false)]);
    temp.cleanup().unwrap();
}

/// No allocation takes a uid a retained reservation names, whichever
/// owner's, one passwd or group names, or one whose home remains.
#[test]
fn an_allocation_avoids_reservations_installed_ids_and_left_homes() {
    let two_owners = Registry::parse(
        "td-principals-v1\nsession\t1000\t993\t992\t991\nsession\t1001\t990\t988\t987\napplication\t1001\tother\t66560\n",
    )
    .unwrap();
    // A ledger that, unlike a seeded one, lacks the reservation's row.
    let (temp, directory) = allocation_fixture(&two_owners, Some(SEEDED));
    let mut prepared = Vec::new();
    assert_eq!(
        allocate_into(&directory, "photo", &mut prepared).unwrap(),
        66561
    );
    temp.cleanup().unwrap();
    let (temp, directory) = allocation_fixture(&Registry::parse(SESSION).unwrap(), Some(SEEDED));
    let installed = || Ok(BTreeSet::from([66560, 66562]));
    assert_eq!(
        directory
            .allocate(1000, "photo", &installable(), installed, |_, _| Ok(()))
            .unwrap(),
        66561
    );
    temp.cleanup().unwrap();
    // A lost ledger forgot 66560 and 66561, whose homes remain; a link or
    // a non-numeric entry names no uid.
    let (temp, directory) = allocation_fixture(&Registry::parse(SESSION).unwrap(), Some(SEEDED));
    let homes = temp.path().join("applications");
    DirBuilder::new().mode(0o755).create(&homes).unwrap();
    for name in ["66560", "66561"] {
        DirBuilder::new()
            .mode(0o700)
            .create(homes.join(name))
            .unwrap();
    }
    fs::write(homes.join(".migration.lock"), b"").unwrap();
    std::os::unix::fs::symlink("66560", homes.join("66563")).unwrap();
    DirBuilder::new()
        .mode(0o700)
        .create(homes.join("066562"))
        .unwrap();
    assert_eq!(
        allocate_into(&directory, "photo", &mut prepared).unwrap(),
        66562
    );
    temp.cleanup().unwrap();
    // A linked `applications` refuses rather than being followed.
    let (temp, directory) = allocation_fixture(&Registry::parse(SESSION).unwrap(), Some(SEEDED));
    let elsewhere = notebook_temp::TempDir::new("td-account-homes").unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), temp.path().join("applications")).unwrap();
    assert!(allocate_into(&directory, "photo", &mut prepared).is_err());
    assert_eq!(account_ledger(&directory).as_deref(), Some(SEEDED));
    temp.cleanup().unwrap();
}

/// An install refuses what a launch would: a retained reservation naming
/// the application at another uid, or a name the image does not declare.
/// Nothing is published or prepared.
#[test]
fn an_allocation_refuses_what_a_launch_refuses() {
    let disagreeing = Registry::parse(
        "td-principals-v1\nsession\t1000\t993\t992\t991\napplication\t1000\tphoto\t66560\n",
    )
    .unwrap();
    let (temp, directory) = allocation_fixture(&disagreeing, Some(SEEDED));
    let mut prepared = Vec::new();
    let refused = allocate_into(&directory, "photo", &mut prepared).unwrap_err();
    assert_eq!(
        refused,
        "application launch and retained assignments disagree"
    );
    assert!(allocate_into(&directory, "other", &mut prepared).is_err());
    assert!(directory
        .allocate(1001, "notes", &installable(), no_ids, |_, _| Ok(()))
        .is_err());
    assert!(prepared.is_empty());
    assert_eq!(account_ledger(&directory).as_deref(), Some(SEEDED));
    // The same rows admitted at launch: the retained uid is the ledger's.
    let agreeing = SEEDED.replace(
        "application\tweb",
        "application\tphoto\t66560\napplication\tweb",
    );
    let (temp2, directory) = allocation_fixture(&disagreeing, Some(&agreeing));
    assert_eq!(
        allocate_into(&directory, "photo", &mut prepared).unwrap(),
        66560
    );
    temp2.cleanup().unwrap();
    temp.cleanup().unwrap();
}

/// Allocations of two names at once serialize on the principal lock and
/// get two uids, both kept.
#[test]
fn concurrent_allocations_of_two_names_take_distinct_uids() {
    let (temp, directory) = allocation_fixture(&Registry::parse(SESSION).unwrap(), Some(SEEDED));
    let barrier = std::sync::Barrier::new(2);
    let (photo, writer) = std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            barrier.wait();
            allocate_into(&directory, "photo", &mut Vec::new()).unwrap()
        });
        let second = scope.spawn(|| {
            barrier.wait();
            allocate_into(&directory, "writer", &mut Vec::new()).unwrap()
        });
        (first.join().unwrap(), second.join().unwrap())
    });
    assert_ne!(photo, writer);
    assert_eq!(
        BTreeSet::from([photo, writer]),
        BTreeSet::from([66560, 66561])
    );
    let ledger = Ledger::parse(&account_ledger(&directory).unwrap()).unwrap();
    assert_eq!(ledger.uid_of(Kind::Application, "photo"), Some(photo));
    assert_eq!(ledger.uid_of(Kind::Application, "writer"), Some(writer));
    temp.cleanup().unwrap();
}

/// Boot seeds the ledger; with none, or with no reservations, an install
/// allocates nothing, publishes nothing and prepares nothing.
#[test]
fn an_allocation_without_a_ledger_refuses() {
    let (temp, directory) = allocation_fixture(&Registry::parse(SESSION).unwrap(), None);
    let mut prepared = Vec::new();
    assert!(allocate_into(&directory, "photo", &mut prepared).is_err());
    assert!(prepared.is_empty());
    assert!(account_ledger(&directory).is_none());
    temp.cleanup().unwrap();
    let temp = notebook_temp::TempDir::new("td-account-allocation").unwrap();
    let directory = account_directory(&temp);
    public_file(&directory.path(workload_accounts::FILE_NAME), SEEDED, 0o644);
    assert!(allocate_into(&directory, "photo", &mut prepared).is_err());
    assert_eq!(account_ledger(&directory).as_deref(), Some(SEEDED));
    temp.cleanup().unwrap();
}

/// A rollback to a deployment that does not declare an installed
/// application, and boot's seeding there, keep its row; that deployment's
/// policy ignores the row, and the newer one finds it again.
#[test]
fn seeding_keeps_an_installed_row_the_policy_does_not_declare() {
    let (temp, directory) = allocation_fixture(&Registry::parse(SESSION).unwrap(), Some(SEEDED));
    allocate_into(&directory, "photo", &mut Vec::new()).unwrap();
    let installed = account_ledger(&directory).unwrap();
    directory
        .enroll(&Registry::parse(SESSION).unwrap())
        .unwrap();
    assert_eq!(account_ledger(&directory).unwrap(), installed);
    let ledger = Ledger::parse(&installed).unwrap();
    let older = crate::app_policy::Declarations::parse(
        "td-bus-applications-v2\t1000\nfirefox\tservice\torg.mozilla.firefox\nweb\tservice\t\n",
    )
    .unwrap();
    let rolled_back = crate::app_policy::Policy::join(older, ledger.clone());
    assert_eq!(rolled_back.account("photo"), None);
    assert!(rolled_back.for_name("photo").is_none());
    assert!(rolled_back.for_uid(66560).is_none());
    let newer = crate::app_policy::Policy::join(installable(), ledger);
    assert_eq!(newer.account("photo"), Some(66560));
    temp.cleanup().unwrap();
}

/// Only the session owner's declared Flathub service applications are
/// allocated: not an undeclared name, a personal application or one the
/// image ships.
#[test]
fn only_a_declared_flathub_service_application_is_allocatable() {
    use crate::app_policy::Declarations;
    let declarations = Declarations::parse(
        "td-bus-applications-v2\t1000\nfirefox\tservice\torg.mozilla.firefox\nmail\tpersonal\t\nphoto\tservice\t\nweb\tservice\t\n",
    )
    .unwrap();
    let flathub: fn(&str) -> bool = |name| ["firefox", "mail", "photo", "other"].contains(&name);
    allocatable(&declarations, 1000, "firefox", flathub).unwrap();
    allocatable(&declarations, 1000, "photo", flathub).unwrap();
    for (owner, name) in [
        (1001, "firefox"),
        (1000, "other"),
        (1000, "mail"),
        (1000, "web"),
    ] {
        assert!(
            allocatable(&declarations, owner, name, flathub).is_err(),
            "{owner} {name}"
        );
    }
    // The image's own declarations: Firefox is its Flathub application.
    allocatable(&declarations, 1000, "firefox", super::flathub).unwrap();
    assert!(allocatable(&declarations, 1000, "photo", super::flathub).is_err());
}

/// What else a new row avoids: every account cgroup at the unified root
/// and every `/run/user` entry, listed without following, in canonical
/// decimal after its prefix; an absent directory names none.
#[test]
fn an_allocation_avoids_uids_with_a_cgroup_or_runtime_left() {
    let temp = notebook_temp::TempDir::new("td-account-leftovers").unwrap();
    let directory = account_directory(&temp);
    assert!(directory.uid_entries("user", "").unwrap().is_empty());
    let users = temp.path().join("user");
    DirBuilder::new().mode(0o755).create(&users).unwrap();
    for name in ["1000", "66560", "066561", "x"] {
        DirBuilder::new()
            .mode(0o700)
            .create(users.join(name))
            .unwrap();
    }
    std::os::unix::fs::symlink("/", users.join("66562")).unwrap();
    assert_eq!(
        directory.uid_entries("user", "").unwrap(),
        BTreeSet::from([1000, 66560, 66562])
    );
    let cgroups = temp.path().join("cgroup");
    DirBuilder::new().mode(0o755).create(&cgroups).unwrap();
    for name in [
        "td-app-66563",
        "td-app-066564",
        "td-app-",
        "td-runtimes",
        "td-user-1000",
        "66565",
    ] {
        DirBuilder::new()
            .mode(0o755)
            .create(cgroups.join(name))
            .unwrap();
    }
    fs::write(cgroups.join("cgroup.procs"), b"").unwrap();
    let delegated = uid_entries(&cgroups, "td-app-").unwrap();
    assert_eq!(delegated, BTreeSet::from([66563]));
    // A linked `user` refuses rather than being listed.
    fs::rename(&users, temp.path().join("users")).unwrap();
    std::os::unix::fs::symlink("users", &users).unwrap();
    assert!(directory.uid_entries("user", "").is_err());
    temp.cleanup().unwrap();

    let (temp, directory) = allocation_fixture(&Registry::parse(SESSION).unwrap(), Some(SEEDED));
    let mut taken = directory_uids_of_a_session();
    taken.extend(delegated.iter().copied());
    let mut prepared = Vec::new();
    let uid = directory
        .allocate(
            1000,
            "photo",
            &installable(),
            || Ok(taken),
            |uid, fresh| {
                prepared.push((uid, fresh));
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(uid, 66564);
    assert_eq!(prepared, [(66564, true)]);
    temp.cleanup().unwrap();
}

/// The runtime directories a session left: 66560 through 66562.
fn directory_uids_of_a_session() -> BTreeSet<u32> {
    BTreeSet::from([1000, 66560, 66561, 66562])
}
