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
        "control",
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
            // A reserved path is an inert row, not a malformed table.
            "control" => (
                "td-grants-v1\napplication\tfirefox\tdirectory\trw\tDown\u{7}loads\n",
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
    let mut delegated = BTreeSet::new();
    uid_entries(&cgroups, "td-app-", &mut delegated).unwrap();
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

/// `directory`'s allocation of the sandbox `name`, avoiding `outside`,
/// recording each preparation's uid and whether its row was new.
fn allocate_sandbox(
    directory: &Directory,
    name: &str,
    outside: BTreeSet<u32>,
    prepared: &mut Vec<(u32, bool)>,
) -> Result<u32, String> {
    directory.allocate_account(
        Kind::Sandbox,
        name,
        || Ok(outside),
        |_, _, _| Ok(()),
        |uid, fresh| {
            prepared.push((uid, fresh));
            Ok(())
        },
    )
}

/// A sandbox's account takes the lowest free uid of 98304 through 131071,
/// never an application's, and is published at the ledger's public mode;
/// a second allocation returns the same row, publishes nothing and
/// prepares again, not as new. A failed preparation keeps the row.
#[test]
fn a_sandbox_allocation_takes_a_workload_uid_once() {
    let (temp, directory) = allocation_fixture(&Registry::parse(SESSION).unwrap(), Some(SEEDED));
    let mut prepared = Vec::new();
    let uid = allocate_sandbox(&directory, "default", BTreeSet::new(), &mut prepared).unwrap();
    assert_eq!(uid, 98304);
    let path = directory.path(workload_accounts::FILE_NAME);
    let published = fs::metadata(&path).unwrap();
    assert_eq!(published.mode() & 0o7777, 0o644);
    assert_eq!(
        account_ledger(&directory).unwrap(),
        format!("{SEEDED}sandbox\tdefault\t98304\n")
    );
    assert!(!directory.path(ACCOUNTS_STAGED).exists());
    assert_eq!(
        allocate_sandbox(&directory, "default", BTreeSet::new(), &mut prepared).unwrap(),
        98304
    );
    assert_eq!(prepared, [(98304, true), (98304, false)]);
    assert_eq!(fs::metadata(&path).unwrap().ino(), published.ino());
    // An application's allocation is untouched by the sandbox's, and the
    // next sandbox takes the next workload uid.
    assert_eq!(
        allocate_into(&directory, "photo", &mut Vec::new()).unwrap(),
        66560
    );
    assert_eq!(
        allocate_sandbox(&directory, "tools", BTreeSet::new(), &mut Vec::new()).unwrap(),
        98305
    );
    assert!(directory
        .allocate_account(
            Kind::Sandbox,
            "broken",
            no_ids,
            |_, _, _| Ok(()),
            |_, _| Err("no cgroup".into()),
        )
        .is_err());
    let ledger = Ledger::parse(&account_ledger(&directory).unwrap()).unwrap();
    assert_eq!(ledger.uid_of(Kind::Sandbox, "broken"), Some(98306));
    assert_eq!(ledger.uid_of(Kind::Sandbox, "default"), Some(98304));
    prepared.clear();
    assert_eq!(
        allocate_sandbox(&directory, "broken", BTreeSet::new(), &mut prepared).unwrap(),
        98306
    );
    assert_eq!(prepared, [(98306, false)]);
    // A refusal by `admit` publishes and prepares nothing.
    let before = account_ledger(&directory);
    assert!(directory
        .allocate_account(
            Kind::Sandbox,
            "refused",
            no_ids,
            |_, _, _| Err("refused".into()),
            |_, _| Err("prepared".into()),
        )
        .is_err());
    assert_eq!(account_ledger(&directory), before);
    temp.cleanup().unwrap();
}

/// A launch's preparation takes the sandbox's existing row, preparing it
/// again under the lock, and never adds one: an absent row, or another
/// kind's of the same name, refuses and leaves the ledger as it was.
#[test]
fn a_launch_prepares_an_existing_sandbox_row_and_adds_none() {
    let (temp, directory) = allocation_fixture(&Registry::parse(SESSION).unwrap(), Some(SEEDED));
    let mut prepared = Vec::new();
    let uid = allocate_sandbox(&directory, "default", BTreeSet::new(), &mut prepared).unwrap();
    let before = account_ledger(&directory);
    let mut again = Vec::new();
    assert_eq!(
        directory
            .prepare_account(Kind::Sandbox, "default", |uid| {
                again.push(uid);
                Ok(())
            })
            .unwrap(),
        uid
    );
    assert_eq!(again, [uid]);
    for (kind, name) in [
        (Kind::Sandbox, "tools"),
        (Kind::Workspace, "default"),
        (Kind::Application, "default"),
    ] {
        let refused = directory
            .prepare_account(kind, name, |_| Err("prepared".into()))
            .unwrap_err();
        assert!(refused.contains("has no account"), "{refused}");
    }
    assert_eq!(account_ledger(&directory), before);
    // A failed preparation is the launch's failure; the row stays.
    assert!(directory
        .prepare_account(Kind::Sandbox, "default", |_| Err("no cgroup".into()))
        .is_err());
    assert_eq!(account_ledger(&directory), before);
    temp.cleanup().unwrap();
}

/// A sandbox's new row avoids every uid another row holds, whatever its
/// kind, every id read outside the state directory (passwd, group, an
/// account cgroup, a runtime directory) and every home left below
/// `workloads` or `applications`; a linked `workloads` refuses.
#[test]
fn a_sandbox_allocation_avoids_rows_ids_and_left_homes() {
    let workspace = format!("{SEEDED}workspace\tagent\t98304\n");
    let (temp, directory) =
        allocation_fixture(&Registry::parse(SESSION).unwrap(), Some(&workspace));
    // A sandbox may share a workspace's name, never its uid.
    assert_eq!(
        allocate_sandbox(&directory, "agent", BTreeSet::new(), &mut Vec::new()).unwrap(),
        98305
    );
    temp.cleanup().unwrap();

    let (temp, directory) = allocation_fixture(&Registry::parse(SESSION).unwrap(), Some(SEEDED));
    assert_eq!(
        allocate_sandbox(
            &directory,
            "default",
            BTreeSet::from([98304, 98306]),
            &mut Vec::new()
        )
        .unwrap(),
        98305
    );
    temp.cleanup().unwrap();

    // A lost ledger forgot 98304 and 98305, whose homes remain, and 98306,
    // left among the application homes; a link or a non-numeric entry
    // names no uid.
    let (temp, directory) = allocation_fixture(&Registry::parse(SESSION).unwrap(), Some(SEEDED));
    for (parent, names) in [
        ("workloads", &["98304", "98305", "098307"][..]),
        ("applications", &["98306"][..]),
    ] {
        let homes = temp.path().join(parent);
        DirBuilder::new().mode(0o755).create(&homes).unwrap();
        for name in names {
            DirBuilder::new()
                .mode(0o700)
                .create(homes.join(name))
                .unwrap();
        }
    }
    std::os::unix::fs::symlink("98304", temp.path().join("workloads/98308")).unwrap();
    let mut prepared = Vec::new();
    assert_eq!(
        allocate_sandbox(&directory, "default", BTreeSet::new(), &mut prepared).unwrap(),
        98307
    );
    assert_eq!(prepared, [(98307, true)]);
    temp.cleanup().unwrap();

    let (temp, directory) = allocation_fixture(&Registry::parse(SESSION).unwrap(), Some(SEEDED));
    let elsewhere = notebook_temp::TempDir::new("td-workload-homes").unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), temp.path().join("workloads")).unwrap();
    assert!(allocate_sandbox(&directory, "default", BTreeSet::new(), &mut Vec::new()).is_err());
    assert_eq!(account_ledger(&directory).as_deref(), Some(SEEDED));
    temp.cleanup().unwrap();

    // With every workload uid taken there is none to give.
    let (temp, directory) = allocation_fixture(&Registry::parse(SESSION).unwrap(), Some(SEEDED));
    let everything: BTreeSet<u32> = workload_accounts::WORKLOAD_UIDS.collect();
    assert!(allocate_sandbox(&directory, "default", everything, &mut Vec::new()).is_err());
    assert_eq!(account_ledger(&directory).as_deref(), Some(SEEDED));
    temp.cleanup().unwrap();
}

/// Only a session owner's sandbox is allocated here: a workspace's waits
/// for step 8, and an application's is the install's.
#[test]
fn only_a_sessions_sandbox_is_allocatable_as_a_workload() {
    let registry = Registry::parse(SESSION).unwrap();
    workload_allocatable(&registry, 1000, Kind::Sandbox).unwrap();
    for (owner, kind) in [
        (1001, Kind::Sandbox),
        (1000, Kind::Workspace),
        (1000, Kind::Application),
    ] {
        assert!(
            workload_allocatable(&registry, owner, kind).is_err(),
            "{owner} {kind:?}"
        );
    }
}

/// A grant request's table: the seed published as boot publishes it.
fn grant_fixture() -> (notebook_temp::TempDir, Directory) {
    let temp = notebook_temp::TempDir::new("td-grant-requests").unwrap();
    let directory = account_directory(&temp);
    directory
        .enroll_checked(&app("mail", 65537), |_| Ok(()))
        .unwrap();
    (temp, directory)
}

fn grant(directory: &Directory, verb: &GrantVerb) -> Result<GrantReply, String> {
    directory.grants(
        Kind::Application,
        "firefox",
        verb,
        || Ok(()),
        Duration::from_secs(1),
    )
}

fn read_only(path: &str) -> GrantVerb {
    GrantVerb::Grant(Access::ReadOnly, path.into())
}

/// The account's directory rows as a listing names them.
fn listed(directory: &Directory) -> Vec<String> {
    match grant(directory, &GrantVerb::List) {
        Ok(GrantReply::List {
            directories,
            network: None,
        }) => directories
            .into_iter()
            .map(|row| format!("{}\t{}", row.access.as_str(), row.path))
            .collect(),
        other => vec![format!("{other:?}")],
    }
}

/// A grant adds a row and publishes the table privately by rename, an
/// equal one publishes nothing, the other access replaces it, and a
/// revoke removes it; a listing reads the account's directory rows.
#[test]
fn grant_requests_list_add_replace_and_revoke_rows() {
    let (temp, directory) = grant_fixture();
    assert_eq!(listed(&directory), ["rw\tDownloads"]);
    let path = directory.path(workload_grants::FILE_NAME);
    let seeded = fs::metadata(&path).unwrap();
    assert_eq!(
        grant(&directory, &read_only("Pictures")).unwrap(),
        GrantReply::Granted(Change::Added)
    );
    let added = fs::metadata(&path).unwrap();
    assert_ne!(added.ino(), seeded.ino());
    assert_eq!((added.mode() & 0o7777, added.nlink()), (0o600, 1));
    assert!(!directory.path(GRANTS_STAGED).exists());
    assert_eq!(
        grant_table(&directory).unwrap(),
        "td-grants-v1\napplication\tfirefox\tdirectory\tro\tPictures\napplication\tfirefox\tdirectory\trw\tDownloads\n"
    );
    assert_eq!(listed(&directory), ["ro\tPictures", "rw\tDownloads"]);
    // Idempotent: the same row again writes nothing.
    assert_eq!(
        grant(&directory, &read_only("Pictures")).unwrap(),
        GrantReply::Granted(Change::Unchanged)
    );
    assert_eq!(fs::metadata(&path).unwrap().ino(), added.ino());
    assert_eq!(
        grant(
            &directory,
            &GrantVerb::Grant(Access::ReadWrite, "Pictures".into())
        )
        .unwrap(),
        GrantReply::Granted(Change::Changed)
    );
    assert_eq!(listed(&directory), ["rw\tDownloads", "rw\tPictures"]);
    assert_eq!(
        grant(&directory, &GrantVerb::Revoke("Pictures".into())).unwrap(),
        GrantReply::Revoked
    );
    assert_eq!(grant_table(&directory).as_deref(), Some(GRANTS_SEED));
    // The seed's own row is revocable, and a revoked row is gone.
    assert_eq!(
        grant(&directory, &GrantVerb::Revoke("Downloads".into())).unwrap(),
        GrantReply::Revoked
    );
    assert_eq!(grant_table(&directory).as_deref(), Some("td-grants-v1\n"));
    assert!(listed(&directory).is_empty());
    let error = grant(&directory, &GrantVerb::Revoke("Downloads".into())).unwrap_err();
    assert!(error.contains("holds no grant"), "{error}");
    temp.cleanup().unwrap();
}

/// What the table refuses a grant refuses and publishes nothing: a
/// reserved, deceptive or overlapping path; a workspace account; an
/// application's network; an admission that refuses; and a missing table.
#[test]
fn grant_requests_refuse_and_change_nothing() {
    let (temp, directory) = grant_fixture();
    let path = directory.path(workload_grants::FILE_NAME);
    let before = fs::metadata(&path).unwrap();
    for refused in [
        ".ssh",
        ".config",
        ".local",
        "Down\u{202E}loads",
        "Downloads/a",
    ] {
        assert!(grant(&directory, &read_only(refused)).is_err(), "{refused}");
    }
    for verb in [
        GrantVerb::List,
        read_only("Pictures"),
        GrantVerb::Revoke("Pictures".into()),
        GrantVerb::Network(Network::Host),
    ] {
        let error = directory
            .grants(
                Kind::Workspace,
                "default",
                &verb,
                || Ok(()),
                Duration::from_secs(1),
            )
            .unwrap_err();
        assert!(error.contains("take no grants"), "{error}");
    }
    // An application's network is its declaration's, never the table's.
    for network in [Network::Host, Network::Off] {
        let error = grant(&directory, &GrantVerb::Network(network)).unwrap_err();
        assert!(error.contains("declaration"), "{error}");
    }
    let error = directory
        .grants(
            Kind::Application,
            "mail",
            &read_only("Pictures"),
            || Err("application \"mail\" runs as you".into()),
            Duration::from_secs(1),
        )
        .unwrap_err();
    assert!(error.contains("runs as you"), "{error}");
    let after = fs::metadata(&path).unwrap();
    assert_eq!(
        (after.ino(), after.mtime(), after.mtime_nsec()),
        (before.ino(), before.mtime(), before.mtime_nsec())
    );
    assert_eq!(grant_table(&directory).as_deref(), Some(GRANTS_SEED));
    // A listing or a revoke admits nothing, so a row an image no longer
    // declares can still be seen and removed.
    let admitted = std::cell::Cell::new(false);
    for verb in [GrantVerb::List, GrantVerb::Revoke("Downloads".into())] {
        directory
            .grants(
                Kind::Application,
                "firefox",
                &verb,
                || {
                    admitted.set(true);
                    Ok(())
                },
                Duration::from_secs(1),
            )
            .unwrap();
    }
    assert!(!admitted.get());
    fs::remove_file(&path).unwrap();
    for verb in [GrantVerb::List, read_only("Pictures")] {
        let error = grant(&directory, &verb).unwrap_err();
        assert!(error.contains("absent"), "{error}");
    }
    assert!(!path.exists());
    temp.cleanup().unwrap();
}

fn sandbox(
    directory: &Directory,
    name: &str,
    verb: &GrantVerb,
    admit: impl FnOnce() -> Result<(), String>,
) -> Result<GrantReply, String> {
    directory.grants(Kind::Sandbox, name, verb, admit, Duration::from_secs(1))
}

/// A sandbox's listing: its directory rows and its network.
fn sandbox_listed(directory: &Directory, name: &str) -> (Vec<String>, Option<Network>) {
    match sandbox(directory, name, &GrantVerb::List, || Err("admitted".into())) {
        Ok(GrantReply::List {
            directories,
            network,
        }) => (
            directories
                .into_iter()
                .map(|row| format!("{}\t{}", row.access.as_str(), row.path))
                .collect(),
            network,
        ),
        other => (vec![format!("{other:?}")], None),
    }
}

/// A sandbox the ledger holds takes directory grants as an application
/// does, home-relative, under the same validator, and its network, which
/// one row holds: host is written, off is no row. A listing names both. No
/// grant adds a ledger row, so none spends the 2048 sandbox and
/// workspace rows allocation alone hands out.
#[test]
fn a_sandbox_the_ledger_holds_takes_grants_and_its_network() {
    let (temp, directory) = grant_fixture();
    allocate_sandbox(&directory, "default", BTreeSet::new(), &mut Vec::new()).unwrap();
    let ledger = account_ledger(&directory);
    let path = directory.path(workload_grants::FILE_NAME);
    assert_eq!(
        sandbox_listed(&directory, "default"),
        (vec![], Some(Network::Off))
    );
    assert_eq!(
        sandbox(&directory, "default", &read_only("src/td"), || Ok(())).unwrap(),
        GrantReply::Granted(Change::Added)
    );
    assert_eq!(
        sandbox(
            &directory,
            "default",
            &GrantVerb::Grant(Access::ReadWrite, "src/td".into()),
            || Ok(())
        )
        .unwrap(),
        GrantReply::Granted(Change::Changed)
    );
    assert_eq!(
        sandbox(
            &directory,
            "default",
            &GrantVerb::Network(Network::Host),
            || Ok(())
        )
        .unwrap(),
        GrantReply::Networked(NetworkChange::Changed)
    );
    assert_eq!(
        grant_table(&directory).unwrap(),
        format!("{GRANTS_SEED}sandbox\tdefault\tdirectory\trw\tsrc/td\nsandbox\tdefault\tnetwork\thost\n")
    );
    assert_eq!(
        sandbox_listed(&directory, "default"),
        (vec!["rw\tsrc/td".to_string()], Some(Network::Host))
    );
    // The same network again writes nothing.
    let published = fs::metadata(&path).unwrap();
    assert_eq!(
        sandbox(
            &directory,
            "default",
            &GrantVerb::Network(Network::Host),
            || Ok(())
        )
        .unwrap(),
        GrantReply::Networked(NetworkChange::Unchanged)
    );
    assert_eq!(fs::metadata(&path).unwrap().ino(), published.ino());
    // An application's listing is untouched by the sandbox's rows.
    assert_eq!(listed(&directory), ["rw\tDownloads"]);
    // What the table refuses an application it refuses a sandbox.
    for refused in [".ssh", ".local", "src/td/a", "Down\u{202E}loads"] {
        assert!(
            sandbox(&directory, "default", &read_only(refused), || Ok(())).is_err(),
            "{refused}"
        );
    }
    // Off removes the row and needs no admission, as a revoke does.
    assert_eq!(
        sandbox(
            &directory,
            "default",
            &GrantVerb::Network(Network::Off),
            || Err("admitted".into())
        )
        .unwrap(),
        GrantReply::Networked(NetworkChange::Changed)
    );
    assert_eq!(
        sandbox(
            &directory,
            "default",
            &GrantVerb::Revoke("src/td".into()),
            || Err("admitted".into())
        )
        .unwrap(),
        GrantReply::Revoked
    );
    assert_eq!(grant_table(&directory).as_deref(), Some(GRANTS_SEED));
    assert_eq!(account_ledger(&directory), ledger);
    temp.cleanup().unwrap();
}

/// A sandbox without a ledger row is granted nothing, its network
/// included, and the table is not written: a grant never allocates. Its
/// admission runs first, so another session's request is refused before
/// the ledger is read. A listing, a revoke and network off name any
/// sandbox.
#[test]
fn a_sandbox_without_an_account_is_granted_nothing() {
    let (temp, directory) = grant_fixture();
    allocate_sandbox(&directory, "default", BTreeSet::new(), &mut Vec::new()).unwrap();
    let ledger = account_ledger(&directory);
    let path = directory.path(workload_grants::FILE_NAME);
    let before = fs::metadata(&path).unwrap();
    for verb in [read_only("src"), GrantVerb::Network(Network::Host)] {
        let error = sandbox(&directory, "tools", &verb, || Ok(())).unwrap_err();
        assert!(error.contains("has no account"), "{error}");
        let error = sandbox(&directory, "default", &verb, || {
            Err("the grant request names another session".into())
        })
        .unwrap_err();
        assert!(error.contains("another session"), "{error}");
    }
    assert_eq!(
        sandbox_listed(&directory, "tools"),
        (vec![], Some(Network::Off))
    );
    assert_eq!(
        sandbox(
            &directory,
            "tools",
            &GrantVerb::Network(Network::Off),
            || Err("admitted".into())
        )
        .unwrap(),
        GrantReply::Networked(NetworkChange::Unchanged)
    );
    let error = sandbox(
        &directory,
        "tools",
        &GrantVerb::Revoke("src".into()),
        || Err("admitted".into()),
    )
    .unwrap_err();
    assert!(error.contains("holds no grant"), "{error}");
    // An application's row is no sandbox's: the ledger holds `mail` as an
    // application, not a sandbox.
    let held = Ledger::parse(&ledger.clone().unwrap()).unwrap();
    assert!(held.uid_of(Kind::Application, "mail").is_some());
    let error = sandbox(&directory, "mail", &read_only("src"), || Ok(())).unwrap_err();
    assert!(error.contains("has no account"), "{error}");
    let after = fs::metadata(&path).unwrap();
    assert_eq!(
        (after.ino(), after.mtime(), after.mtime_nsec()),
        (before.ino(), before.mtime(), before.mtime_nsec())
    );
    assert_eq!(grant_table(&directory).as_deref(), Some(GRANTS_SEED));
    assert_eq!(account_ledger(&directory), ledger);
    temp.cleanup().unwrap();
}

/// A sandbox's grant names the primary human's session, which the
/// principal table holds: a grant's source is that human's home, so a
/// second session owner, even one the table names, is refused.
#[test]
fn a_sandbox_grant_names_the_primary_session() {
    let registry = Registry::parse(SESSION).unwrap();
    sandbox_grantable(&registry, 1000).unwrap();
    let error = sandbox_grantable(&registry, 1001).unwrap_err();
    assert!(error.contains("another session"), "{error}");
    let second = Registry::parse(&format!("{SESSION}session\t1001\t990\t989\t988\n")).unwrap();
    assert!(second.sessions().any(|session| session.owner == 1001));
    sandbox_grantable(&second, 1000).unwrap();
    let error = sandbox_grantable(&second, 1001).unwrap_err();
    assert!(error.contains("another session"), "{error}");
    // Nor is the primary admitted where the table names no such session.
    let other = Registry::parse("td-principals-v1\nsession\t1001\t990\t989\t988\n").unwrap();
    assert!(sandbox_grantable(&other, 1000).is_err());
}

/// A change waits for the principal lock only so long, then answers that
/// the table is busy; a listing takes no lock.
#[test]
fn a_grant_request_waits_for_the_lock_only_briefly() {
    let (temp, directory) = grant_fixture();
    let held = directory.lock().unwrap();
    let started = Instant::now();
    let error = directory
        .grants(
            Kind::Application,
            "firefox",
            &read_only("Pictures"),
            || Ok(()),
            Duration::from_millis(50),
        )
        .unwrap_err();
    assert!(error.contains("busy"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(listed(&directory), ["rw\tDownloads"]);
    assert_eq!(grant_table(&directory).as_deref(), Some(GRANTS_SEED));
    drop(held);
    assert_eq!(
        grant(&directory, &read_only("Pictures")).unwrap(),
        GrantReply::Granted(Change::Added)
    );
    temp.cleanup().unwrap();
}

/// Only a declared service application of the session's owner takes a
/// grant: not an undeclared name, a personal application or another
/// session's.
#[test]
fn only_a_declared_service_application_is_grantable() {
    let declarations = installable();
    for name in ["firefox", "web", "photo"] {
        grantable(&declarations, 1000, name).unwrap();
    }
    let error = grantable(&declarations, 1000, "mail").unwrap_err();
    assert!(error.contains("runs as you"), "{error}");
    assert!(grantable(&declarations, 1000, "other").is_err());
    assert!(grantable(&declarations, 1001, "firefox").is_err());
}

/// The early source check: a directory of the owner's, each name on the
/// way, the home's included, reached without a link and writable by no
/// one else.
#[test]
fn a_grant_source_is_an_existing_private_directory_reached_without_links() {
    use std::os::unix::fs::symlink;
    let temp = notebook_temp::TempDir::new("td-grant-source").unwrap();
    let home = temp.path().join("home");
    DirBuilder::new().mode(0o700).create(&home).unwrap();
    let metadata = fs::metadata(&home).unwrap();
    let owner = (metadata.uid(), metadata.gid());
    for directory in ["Pictures", "Pictures/2026", "Shared"] {
        DirBuilder::new()
            .mode(0o755)
            .create(home.join(directory))
            .unwrap();
    }
    fs::set_permissions(home.join("Shared"), Permissions::from_mode(0o775)).unwrap();
    public_file(&home.join("file"), "", 0o600);
    symlink("Pictures", home.join("link")).unwrap();
    grant_source(&home, "Pictures", owner).unwrap();
    grant_source(&home, "Pictures/2026", owner).unwrap();
    for (path, shown, why) in [
        ("Missing", "~/Missing", "does not exist"),
        ("Pictures/missing", "~/Pictures/missing", "does not exist"),
        ("file", "~/file", "not a directory"),
        ("link", "~/link", "not a directory"),
        ("link/2026", "~/link", "not a directory"),
        ("Shared", "~/Shared", "alone"),
    ] {
        let error = grant_source(&home, path, owner).unwrap_err();
        assert!(
            error.starts_with(shown) && error.contains(why),
            "{path}: {error}"
        );
    }
    let foreign = (owner.0.wrapping_add(1), owner.1);
    assert!(grant_source(&home, "Pictures", foreign)
        .unwrap_err()
        .starts_with("the home"));
    fs::set_permissions(&home, Permissions::from_mode(0o770)).unwrap();
    assert!(grant_source(&home, "Pictures", owner)
        .unwrap_err()
        .starts_with("the home"));
    fs::set_permissions(&home, Permissions::from_mode(0o700)).unwrap();
    temp.cleanup().unwrap();
}

/// A row a later validator refuses stays where an earlier deployment put
/// it: listed with the account's other rows and revocable, while a grant
/// of its path is refused before the request is admitted.
#[test]
fn an_inert_row_is_listed_and_revocable_but_never_granted() {
    let (temp, directory) = grant_fixture();
    let path = directory.path(workload_grants::FILE_NAME);
    let staged = directory.path("inert");
    public_file(
        &staged,
        "td-grants-v1\napplication\tfirefox\tdirectory\tro\t.ssh\napplication\tfirefox\tdirectory\trw\tDownloads\n",
        0o600,
    );
    fs::rename(&staged, &path).unwrap();
    assert_eq!(listed(&directory), ["ro\t.ssh", "rw\tDownloads"]);
    let refused = directory
        .grants(
            Kind::Application,
            "firefox",
            &read_only(".ssh/keys"),
            || Err("admitted".into()),
            Duration::from_secs(1),
        )
        .unwrap_err();
    assert!(refused.contains("reserved"), "{refused}");
    assert!(matches!(
        grant(&directory, &GrantVerb::Revoke(".ssh".into())),
        Ok(GrantReply::Revoked)
    ));
    assert_eq!(listed(&directory), ["rw\tDownloads"]);
    // A path no row can hold is refused before the lock.
    assert!(grant(&directory, &GrantVerb::Revoke("Down\u{7}loads".into())).is_err());
    temp.cleanup().unwrap();
}

/// A reader without the lock that opened the table just before a publish
/// renamed its successor over it holds the whole earlier table, at no
/// link: the listing takes it, a locked reader still refuses it.
#[test]
fn an_unlocked_listing_takes_a_table_renamed_over_while_it_read() {
    let (temp, directory) = grant_fixture();
    let path = directory.path(workload_grants::FILE_NAME);
    let opened = File::open(&path).unwrap();
    let staged = directory.path("successor");
    public_file(&staged, GRANTS_SEED, 0o600);
    fs::rename(&staged, &path).unwrap();
    assert_eq!(opened.metadata().unwrap().nlink(), 0);
    let name = workload_grants::FILE_NAME;
    directory
        .validate_links(&opened, name, GRANTS_MODE, GRANTS_LIMIT, true)
        .unwrap();
    assert!(directory
        .validate_links(&opened, name, GRANTS_MODE, GRANTS_LIMIT, false)
        .is_err());
    temp.cleanup().unwrap();
}
