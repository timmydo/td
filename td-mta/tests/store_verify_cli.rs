#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::{
    fs,
    os::unix::{
        ffi::OsStringExt,
        fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
    },
    path::PathBuf,
    process::{Command, Output},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use td_mta::{
    clock::RuntimeClock,
    format::{
        key::Key,
        operation::Operation,
        row::{BlobRow, MailboxRow, Row},
        Sequence, Table,
    },
    ids::{AccountId, BlobId, MailboxId, StoreEpoch},
    ports::{Clock, Crypto, Deadline, Digest, ReadView},
    store_fs::{BlobSource, CommitRequest, IndexStore, PrivateRoot},
};

const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
const OTHER: AccountId = AccountId::from_bytes([2; 16]);
const BLOB: BlobId = BlobId::from_bytes([3; 16]);
const MARKER: &[u8] = b"td-mail-verify-selected-body-marker";
const OTHER_MARKER: &[u8] = b"td-mail-verify-other-body-marker";

// Re-execute each case under the existing capped, private filesystem fixture.
// Its default shared-write modes suit other crates; mail needs strict ancestors.
fn private_case(name: &str) -> bool {
    const CHILD: &str = "TD_MAIL_VERIFY_PRIVATE_CASE";
    if std::env::var(CHILD).ok().as_deref() == Some(name) {
        let uid = fs::metadata("/proc/self").unwrap().uid();
        assert_ne!(uid, 0);
        assert_eq!(fs::metadata("/").unwrap().uid(), uid);
        assert_eq!(fs::metadata("/tmp").unwrap().uid(), uid);
        let mounts = fs::read_to_string("/proc/self/mountinfo").unwrap();
        assert!(mounts.lines().any(|line| {
            line.split_whitespace().nth(4) == Some("/")
                && line
                    .split_once(" - ")
                    .is_some_and(|(_, fs)| fs.starts_with("tmpfs "))
        }));
        assert!(std::env::var_os("TD_TEST_TRUSTED_ROOT").is_none());
        fs::set_permissions("/", fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions("/tmp", fs::Permissions::from_mode(0o755)).unwrap();
        return false;
    }
    let builder = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target/release/td-builder");
    let output = Command::new(builder)
        .arg("run-capped")
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("TD_TEST_TRUSTED_ROOT", "1")
        .env(CHILD, name)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("test result: ok. 1 passed; 0 failed;"),
        "{stdout}"
    );
    true
}

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir();
        let path = base.join(format!(
            "verify-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(fs::canonicalize(path).unwrap())
    }

    fn seed(&self) {
        let mut root = PrivateRoot::open(self.0.to_str().unwrap())
            .unwrap()
            .try_lock()
            .unwrap();
        let clock: Arc<dyn Clock> = Arc::new(RuntimeClock::new());
        let deadline = Deadline::after(clock.sample().unwrap().monotonic, 60_000).unwrap();
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            clock,
            1,
            deadline,
        )
        .unwrap();
        for (account, byte, marker, length) in [
            (ACCOUNT, 0x5a, MARKER, 65_553),
            (OTHER, 0xa5, OTHER_MARKER, 43),
        ] {
            store.create_account(account, deadline).unwrap();
            let mut bytes = vec![byte; length];
            bytes[..marker.len()].copy_from_slice(marker);
            let mut hash = td_crypto::Provider.sha256().unwrap();
            hash.update(&bytes).unwrap();
            let row = Row::Blob(BlobRow {
                length: bytes.len() as u64,
                digest: hash.finish().unwrap(),
                created_at: 17,
            });
            let mut value = vec![0; row.encoded_len().unwrap()];
            row.encode(&mut value).unwrap();
            let mut source = bytes.as_slice();
            store
                .commit(
                    &td_crypto::Provider,
                    CommitRequest {
                        account,
                        epoch: store.epoch(),
                        expected: Sequence::from_u64(0),
                        utc_ms: 17,
                        deadline,
                    },
                    &[Operation::put(Table::Blobs, BLOB.as_bytes(), &value).unwrap()],
                    &mut [BlobSource {
                        id: BLOB,
                        source: &mut source,
                    }],
                )
                .unwrap();
        }
        store.checkpoint(deadline).unwrap();
    }
    fn command(&self, account: AccountId) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_td-mta"));
        command
            .args(["store", "verify", "--root"])
            .arg(&self.0)
            .args(["--account", &account.to_string()]);
        command
    }
    fn verify(&self, account: AccountId) -> Output {
        self.command(account).output().unwrap()
    }
    fn backup_command(&self, destination: &Self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_td-mta"));
        command
            .args(["backup", "--root"])
            .arg(&self.0)
            .arg("--destination")
            .arg(&destination.0);
        command
    }
    fn corrupt(&self, marker: &[u8]) {
        let path = self.0.join("metadata.sqlite3");
        let mut bytes = fs::read(&path).unwrap();
        let matches: Vec<_> = bytes
            .windows(marker.len())
            .enumerate()
            .filter_map(|(i, bytes)| (bytes == marker).then_some(i))
            .collect();
        assert_eq!(matches.len(), 1);
        bytes[matches[0]] ^= 1;
        fs::write(path, bytes).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn report(output: &Output, code: i32) -> td_json::Json {
    report_command(output, code, "store.verify")
}
fn report_command(output: &Output, code: i32, command: &str) -> td_json::Json {
    assert_eq!(
        output.status.code(),
        Some(code),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 1);
    let json = td_json::parse(stdout).unwrap();
    assert_eq!(json.get("schema").unwrap().as_u64(), Some(1));
    assert_eq!(json.get("command").unwrap().as_str(), Some(command));
    json
}

#[test]
fn command_verifies_real_chunks_and_identifies_only_the_selected_account() {
    if private_case("command_verifies_real_chunks_and_identifies_only_the_selected_account") {
        return;
    }
    let fixture = Fixture::new();
    fixture.seed();
    let json = report(
        &fixture
            .command(ACCOUNT)
            .args(["--timeout-seconds", "60"])
            .output()
            .unwrap(),
        0,
    );
    assert_eq!(json.get("status").unwrap().as_str(), Some("ok"));
    assert_eq!(json.get("scope").unwrap().as_str(), Some("account"));
    assert_eq!(
        json.get("physical_integrity"),
        Some(&td_json::Json::Bool(true))
    );
    assert_eq!(
        json.get("account").unwrap().as_str(),
        Some(ACCOUNT.to_string().as_str())
    );
    assert_eq!(
        json.get("epoch").unwrap().as_str(),
        Some(StoreEpoch::from_bytes([9; 16]).to_string().as_str())
    );
    for (field, expected) in [
        ("sequence", 1),
        ("history_floor", 0),
        ("metadata_rows", 1),
        ("mailboxes", 0),
        ("submissions", 0),
        ("recipients", 0),
        ("blobs", 1),
        ("body_bytes", 65_553),
    ] {
        assert_eq!(json.get(field).unwrap().as_u64(), Some(expected), "{field}");
    }
    assert!(json.get("checked_at_ms").unwrap().as_u64().unwrap() > 0);
    fixture.corrupt(OTHER_MARKER);
    // Other accounts' digests are outside the explicitly reported account scope.
    let selected = report(&fixture.verify(ACCOUNT), 0);
    assert_eq!(selected.get("body_bytes").unwrap().as_u64(), Some(65_553));
    let other = report(&fixture.verify(OTHER), 1);
    assert_eq!(other.get("stage").unwrap().as_str(), Some("bodies"));
    assert_eq!(other.get("error").unwrap().as_str(), Some("corrupt"));
    let mut root = PrivateRoot::open(fixture.0.to_str().unwrap())
        .unwrap()
        .try_lock()
        .unwrap();
    let clock: Arc<dyn Clock> = Arc::new(RuntimeClock::new());
    let deadline = Deadline::after(clock.sample().unwrap().monotonic, 60_000).unwrap();
    let store = IndexStore::open(&mut root, clock, 1, deadline).unwrap();
    let view = store.view(ACCOUNT, deadline).unwrap();
    assert_eq!(view.identity().committed_sequence, Sequence::from_u64(1));
    assert_eq!(view.identity().epoch, StoreEpoch::from_bytes([9; 16]));
}

#[test]
fn digest_and_physical_failures_never_report_success() {
    if private_case("digest_and_physical_failures_never_report_success") {
        return;
    }
    let fixture = Fixture::new();
    fixture.seed();
    fixture.corrupt(MARKER);
    let json = report(&fixture.verify(ACCOUNT), 1);
    assert_eq!(json.get("stage").unwrap().as_str(), Some("bodies"));
    assert_eq!(json.get("error").unwrap().as_str(), Some("corrupt"));
    assert!(json.get("physical_integrity").is_none());
    let path = fixture.0.join("metadata.sqlite3");
    let mut bytes = fs::read(&path).unwrap();
    bytes[0] = 0;
    fs::write(path, bytes).unwrap();
    let json = report(&fixture.verify(ACCOUNT), 1);
    assert!(matches!(
        json.get("stage").unwrap().as_str(),
        Some("open") | Some("physical")
    ));
    assert_eq!(json.get("status").unwrap().as_str(), Some("error"));
}

#[test]
fn active_store_missing_account_and_private_root_refuse_with_machine_results() {
    if private_case("active_store_missing_account_and_private_root_refuse_with_machine_results") {
        return;
    }
    let fixture = Fixture::new();
    fixture.seed();
    let root = PrivateRoot::open(fixture.0.to_str().unwrap())
        .unwrap()
        .try_lock()
        .unwrap();
    let json = report(&fixture.verify(ACCOUNT), 1);
    assert_eq!(json.get("stage").unwrap().as_str(), Some("lock"));
    assert_eq!(json.get("error").unwrap().as_str(), Some("busy"));
    drop(root);
    let json = report(&fixture.verify(AccountId::from_bytes([4; 16])), 1);
    assert_eq!(json.get("error").unwrap().as_str(), Some("not-found"));
    fs::set_permissions(&fixture.0, fs::Permissions::from_mode(0o755)).unwrap();
    let json = report(&fixture.verify(ACCOUNT), 1);
    assert_eq!(json.get("stage").unwrap().as_str(), Some("root"));
    fs::set_permissions(&fixture.0, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn malformed_options_are_rejected_before_store_access() {
    if private_case("malformed_options_are_rejected_before_store_access") {
        return;
    }
    let fixture = Fixture::new();
    for extra in [
        vec!["--timeout-seconds", "0"],
        vec!["--timeout-seconds", "-1"],
        vec!["--timeout-seconds", "+1"],
        vec!["--timeout-seconds", "18446744073709551615"],
        vec!["--timeout-seconds"],
        vec!["--root", "duplicate"],
        vec!["--account", "duplicate"],
        vec!["--unknown", "x"],
    ] {
        let json = report(&fixture.command(ACCOUNT).args(extra).output().unwrap(), 2);
        assert_eq!(json.get("stage").unwrap().as_str(), Some("arguments"));
        assert!(!fixture.0.join("LOCK").exists());
    }
    for args in [
        vec!["store", "verify"],
        vec!["store", "verify", "--account", "ABC"],
    ] {
        let json = report(
            &Command::new(env!("CARGO_BIN_EXE_td-mta"))
                .args(args)
                .output()
                .unwrap(),
            2,
        );
        assert_eq!(
            json.get("error").unwrap().as_str(),
            Some("invalid-arguments")
        );
    }
    for args in [vec!["store"], vec!["store", "repair"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_td-mta"))
            .args(args)
            .output()
            .unwrap();
        let json = report_command(&output, 2, "store");
        assert_eq!(
            json.get("error").unwrap().as_str(),
            Some("invalid-arguments")
        );
    }
    let output = fixture
        .command(ACCOUNT)
        .arg("--timeout-seconds")
        .arg(std::ffi::OsString::from_vec(vec![0xff]))
        .output()
        .unwrap();
    let json = report(&output, 2);
    assert_eq!(json.get("stage").unwrap().as_str(), Some("arguments"));
    assert!(!fixture.0.join("LOCK").exists());
}

#[test]
fn backup_copies_all_accounts_without_overwrite_or_semantic_claim() {
    if private_case("backup_copies_all_accounts_without_overwrite_or_semantic_claim") {
        return;
    }
    let source = Fixture::new();
    source.seed();
    let destination = Fixture::new();
    let output = source
        .backup_command(&destination)
        .args(["--timeout-seconds", "60"])
        .output()
        .unwrap();
    let json = report_command(&output, 0, "backup");
    assert_eq!(json.get("status").unwrap().as_str(), Some("ok"));
    assert_eq!(json.get("scope").unwrap().as_str(), Some("database"));
    assert_eq!(
        json.get("semantic_verified"),
        Some(&td_json::Json::Bool(false))
    );
    assert_eq!(
        json.get("epoch").unwrap().as_str(),
        Some(StoreEpoch::from_bytes([9; 16]).to_string().as_str())
    );
    let path = destination.0.join("metadata.sqlite3");
    let copied = fs::read(&path).unwrap();
    assert_eq!(
        json.get("bytes").unwrap().as_u64(),
        Some(copied.len() as u64)
    );
    assert_eq!(copied, fs::read(source.0.join("metadata.sqlite3")).unwrap());
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o600);
    assert!(!destination
        .0
        .join("metadata.sqlite3.backup-partial")
        .exists());
    for (account, bytes) in [(ACCOUNT, 65_553), (OTHER, 43)] {
        let checked = report(&destination.verify(account), 0);
        assert_eq!(checked.get("body_bytes").unwrap().as_u64(), Some(bytes));
        assert_eq!(checked.get("sequence").unwrap().as_u64(), Some(1));
        assert_eq!(checked.get("epoch"), json.get("epoch"));
    }
    let before_refusal = fs::read(&path).unwrap();
    let output = source.backup_command(&destination).output().unwrap();
    let refused = report_command(&output, 1, "backup");
    assert_eq!(refused.get("error").unwrap().as_str(), Some("conflict"));
    assert_eq!(
        refused.get("publication").unwrap().as_str(),
        Some("unpublished")
    );
    assert_eq!(fs::read(path).unwrap(), before_refusal);
    report(&source.verify(ACCOUNT), 0);

    // A consistent backup preserves damage; its receipt is not verification.
    source.corrupt(MARKER);
    let damaged = Fixture::new();
    let output = source.backup_command(&damaged).output().unwrap();
    let copied_damage = report_command(&output, 0, "backup");
    assert_eq!(
        copied_damage.get("semantic_verified"),
        Some(&td_json::Json::Bool(false))
    );
    let failed_check = report(&damaged.verify(ACCOUNT), 1);
    assert_eq!(failed_check.get("stage").unwrap().as_str(), Some("bodies"));
    assert_eq!(failed_check.get("error").unwrap().as_str(), Some("corrupt"));
}

#[test]
fn backup_refuses_busy_or_invalid_roots_without_publication() {
    if private_case("backup_refuses_busy_or_invalid_roots_without_publication") {
        return;
    }
    let source = Fixture::new();
    source.seed();
    let destination = Fixture::new();
    for (fixture, stage) in [(&source, "source-lock"), (&destination, "destination-lock")] {
        let lock = PrivateRoot::open(fixture.0.to_str().unwrap())
            .unwrap()
            .try_lock()
            .unwrap();
        let output = source.backup_command(&destination).output().unwrap();
        let json = report_command(&output, 1, "backup");
        assert_eq!(json.get("stage").unwrap().as_str(), Some(stage));
        assert_eq!(json.get("error").unwrap().as_str(), Some("busy"));
        assert_eq!(
            json.get("publication").unwrap().as_str(),
            Some("unpublished")
        );
        drop(lock);
        assert!(!destination.0.join("metadata.sqlite3").exists());
        assert!(!destination
            .0
            .join("metadata.sqlite3.backup-partial")
            .exists());
    }
    fs::set_permissions(&destination.0, fs::Permissions::from_mode(0o755)).unwrap();
    let output = source.backup_command(&destination).output().unwrap();
    let json = report_command(&output, 1, "backup");
    assert_eq!(
        json.get("stage").unwrap().as_str(),
        Some("destination-root")
    );
    assert_eq!(
        json.get("publication").unwrap().as_str(),
        Some("unpublished")
    );
    fs::set_permissions(&destination.0, fs::Permissions::from_mode(0o700)).unwrap();
    let original = fs::read(source.0.join("metadata.sqlite3")).unwrap();
    let output = source.backup_command(&source).output().unwrap();
    let json = report_command(&output, 1, "backup");
    assert_eq!(
        json.get("publication").unwrap().as_str(),
        Some("unpublished")
    );
    assert_eq!(
        fs::read(source.0.join("metadata.sqlite3")).unwrap(),
        original
    );
    report(&source.verify(ACCOUNT), 0);
}

#[test]
fn backup_arguments_refuse_before_either_root_is_accessed() {
    if private_case("backup_arguments_refuse_before_either_root_is_accessed") {
        return;
    }
    let source = Fixture::new();
    let destination = Fixture::new();
    for extra in [
        vec!["--root", "duplicate"],
        vec!["--destination", "duplicate"],
        vec!["--timeout-seconds", "0"],
        vec!["--timeout-seconds", "18446744073709551615"],
        vec!["--account", "invalid-for-backup"],
    ] {
        let output = source
            .backup_command(&destination)
            .args(extra)
            .output()
            .unwrap();
        let json = report_command(&output, 2, "backup");
        assert_eq!(json.get("stage").unwrap().as_str(), Some("arguments"));
        assert_eq!(
            json.get("error").unwrap().as_str(),
            Some("invalid-arguments")
        );
        assert!(!source.0.join("LOCK").exists());
        assert!(!destination.0.join("LOCK").exists());
    }
    let output = Command::new(env!("CARGO_BIN_EXE_td-mta"))
        .args(["backup", "--root"])
        .arg(&source.0)
        .output()
        .unwrap();
    let json = report_command(&output, 2, "backup");
    assert_eq!(json.get("stage").unwrap().as_str(), Some("arguments"));
    let output = source
        .backup_command(&destination)
        .arg(std::ffi::OsString::from_vec(vec![0xff]))
        .arg("x")
        .output()
        .unwrap();
    report_command(&output, 2, "backup");
    assert!(!source.0.join("LOCK").exists());
    assert!(!destination.0.join("LOCK").exists());
}

#[test]
fn whole_database_verification_checks_every_account_before_success() {
    if private_case("whole_database_verification_checks_every_account_before_success") {
        return;
    }
    let fixture = Fixture::new();
    fixture.seed();
    let all = || {
        Command::new(env!("CARGO_BIN_EXE_td-mta"))
            .args(["store", "verify", "--all", "--root"])
            .arg(&fixture.0)
            .args(["--timeout-seconds", "60"])
            .output()
            .unwrap()
    };
    let json = report(&all(), 0);
    assert_eq!(json.get("scope").unwrap().as_str(), Some("database"));
    assert_eq!(
        json.get("physical_integrity"),
        Some(&td_json::Json::Bool(true))
    );
    assert_eq!(
        json.get("epoch").unwrap().as_str(),
        Some(StoreEpoch::from_bytes([9; 16]).to_string().as_str())
    );
    for (field, count) in [
        ("accounts", 2),
        ("metadata_rows", 2),
        ("mailboxes", 0),
        ("submissions", 0),
        ("recipients", 0),
        ("blobs", 2),
        ("body_bytes", 65_596),
    ] {
        assert_eq!(json.get(field).unwrap().as_u64(), Some(count), "{field}");
    }
    for field in ["account", "sequence", "history_floor"] {
        assert!(json.get(field).is_none(), "{field}");
    }
    assert!(json.get("checked_at_ms").unwrap().as_u64().unwrap() > 0);
    fixture.corrupt(OTHER_MARKER);
    report(&fixture.verify(ACCOUNT), 0);
    let failed = report(&all(), 1);
    assert_eq!(failed.get("stage").unwrap().as_str(), Some("bodies"));
    assert_eq!(failed.get("error").unwrap().as_str(), Some("corrupt"));
    for field in [
        "accounts",
        "metadata_rows",
        "blobs",
        "body_bytes",
        "physical_integrity",
        "epoch",
    ] {
        assert!(failed.get(field).is_none(), "{field}");
    }
}

#[test]
fn whole_database_verification_accepts_empty_and_refuses_invalid_selection() {
    if private_case("whole_database_verification_accepts_empty_and_refuses_invalid_selection") {
        return;
    }
    let fixture = Fixture::new();
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_td-mta"));
        command.args(["store", "verify", "--root"]).arg(&fixture.0);
        command
    };
    let account = ACCOUNT.to_string();
    for flags in [
        vec!["--all", "--all"],
        vec!["--all", "--account", account.as_str()],
        vec!["--account", account.as_str(), "--all"],
        vec!["--all", "value"],
        vec![],
    ] {
        let failed = report(&command().args(flags).output().unwrap(), 2);
        assert_eq!(
            failed.get("error").unwrap().as_str(),
            Some("invalid-arguments")
        );
        assert!(!fixture.0.join("LOCK").exists());
    }
    {
        let mut root = PrivateRoot::open(fixture.0.to_str().unwrap())
            .unwrap()
            .try_lock()
            .unwrap();
        let clock: Arc<dyn Clock> = Arc::new(RuntimeClock::new());
        let deadline = Deadline::after(clock.sample().unwrap().monotonic, 60_000).unwrap();
        let _store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            clock,
            1,
            deadline,
        )
        .unwrap();
    }
    let json = report(&command().arg("--all").output().unwrap(), 0);
    assert_eq!(json.get("scope").unwrap().as_str(), Some("database"));
    for field in [
        "accounts",
        "metadata_rows",
        "mailboxes",
        "submissions",
        "recipients",
        "blobs",
        "body_bytes",
    ] {
        assert_eq!(json.get(field).unwrap().as_u64(), Some(0), "{field}");
    }
    let held = PrivateRoot::open(fixture.0.to_str().unwrap())
        .unwrap()
        .try_lock()
        .unwrap();
    let failed = report(&command().arg("--all").output().unwrap(), 1);
    assert_eq!(failed.get("stage").unwrap().as_str(), Some("lock"));
    assert_eq!(failed.get("error").unwrap().as_str(), Some("busy"));
    drop(held);
    let path = fixture.0.join("metadata.sqlite3");
    let mut bytes = fs::read(&path).unwrap();
    bytes[0] = 0;
    fs::write(path, bytes).unwrap();
    let failed = report(&command().arg("--all").output().unwrap(), 1);
    assert!(matches!(
        failed.get("stage").unwrap().as_str(),
        Some("open") | Some("physical")
    ));
}

fn restore_command(source: &Fixture, destination: &Fixture) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_td-mta"));
    command
        .args(["restore", "--root"])
        .arg(&source.0)
        .arg("--destination")
        .arg(&destination.0);
    command
}

#[test]
fn restore_verifies_both_accounts_and_persists_a_destination_only_fresh_epoch() {
    if private_case("restore_verifies_both_accounts_and_persists_a_destination_only_fresh_epoch") {
        return;
    }
    let source = Fixture::new();
    source.seed();
    let destination = Fixture::new();
    let json = report_command(
        &restore_command(&source, &destination)
            .args(["--timeout-seconds", "60"])
            .output()
            .unwrap(),
        0,
        "restore",
    );
    assert_eq!(json.get("status").unwrap().as_str(), Some("ok"));
    assert_eq!(json.get("scope").unwrap().as_str(), Some("database"));
    assert_eq!(json.get("service_ready"), Some(&td_json::Json::Bool(false)));
    assert_eq!(
        json.get("physical_integrity"),
        Some(&td_json::Json::Bool(true))
    );
    let original = StoreEpoch::from_bytes([9; 16]).to_string();
    assert_eq!(
        json.get("source_epoch").unwrap().as_str(),
        Some(original.as_str())
    );
    let epoch = json.get("epoch").unwrap().as_str().unwrap();
    StoreEpoch::parse(epoch).unwrap();
    assert_ne!(epoch, original);
    for (field, count) in [
        ("accounts", 2),
        ("metadata_rows", 2),
        ("mailboxes", 0),
        ("submissions", 0),
        ("recipients", 0),
        ("blobs", 2),
        ("body_bytes", 65_596),
    ] {
        assert_eq!(json.get(field).unwrap().as_u64(), Some(count), "{field}");
    }
    assert!(json.get("copied_bytes").unwrap().as_u64().unwrap() > 0);
    assert!(json.get("checked_at_ms").unwrap().as_u64().unwrap() > 0);
    assert_eq!(
        fs::metadata(destination.0.join("metadata.sqlite3"))
            .unwrap()
            .mode()
            & 0o7777,
        0o600
    );
    assert!(!destination
        .0
        .join("metadata.sqlite3.backup-partial")
        .exists());
    for (account, bytes) in [(ACCOUNT, 65_553), (OTHER, 43)] {
        let copied = report(&destination.verify(account), 0);
        assert_eq!(copied.get("epoch").unwrap().as_str(), Some(epoch));
        assert_eq!(copied.get("body_bytes").unwrap().as_u64(), Some(bytes));
        assert_eq!(copied.get("sequence").unwrap().as_u64(), Some(1));
        assert_eq!(copied.get("history_floor").unwrap().as_u64(), Some(0));
        let retained = report(&source.verify(account), 0);
        assert_eq!(
            retained.get("epoch").unwrap().as_str(),
            Some(original.as_str())
        );
        assert_eq!(retained.get("body_bytes").unwrap().as_u64(), Some(bytes));
        assert_eq!(retained.get("sequence").unwrap().as_u64(), Some(1));
    }
    let before = fs::read(destination.0.join("metadata.sqlite3")).unwrap();
    let refused = report_command(
        &restore_command(&source, &destination).output().unwrap(),
        1,
        "restore",
    );
    assert_eq!(refused.get("error").unwrap().as_str(), Some("conflict"));
    assert_eq!(
        refused.get("progress").unwrap().as_str(),
        Some("unpublished")
    );
    assert_eq!(
        fs::read(destination.0.join("metadata.sqlite3")).unwrap(),
        before
    );
    let another = Fixture::new();
    let second = report_command(
        &restore_command(&source, &another).output().unwrap(),
        0,
        "restore",
    );
    // Sample inequality checks wiring, not entropy quality.
    assert_ne!(second.get("epoch").unwrap().as_str(), Some(epoch));
    assert_ne!(
        second.get("epoch").unwrap().as_str(),
        Some(original.as_str())
    );
}

#[test]
fn restore_verification_failure_keeps_an_inspectable_copy_under_its_original_epoch() {
    if private_case(
        "restore_verification_failure_keeps_an_inspectable_copy_under_its_original_epoch",
    ) {
        return;
    }
    let source = Fixture::new();
    source.seed();
    source.corrupt(OTHER_MARKER);
    let destination = Fixture::new();
    let failed = report_command(
        &restore_command(&source, &destination).output().unwrap(),
        1,
        "restore",
    );
    assert_eq!(failed.get("stage").unwrap().as_str(), Some("bodies"));
    assert_eq!(failed.get("error").unwrap().as_str(), Some("corrupt"));
    assert_eq!(failed.get("progress").unwrap().as_str(), Some("copied"));
    for field in [
        "epoch",
        "source_epoch",
        "accounts",
        "body_bytes",
        "physical_integrity",
        "service_ready",
    ] {
        assert!(failed.get(field).is_none(), "{field}");
    }
    assert_eq!(
        fs::read(destination.0.join("metadata.sqlite3")).unwrap(),
        fs::read(source.0.join("metadata.sqlite3")).unwrap()
    );
    let healthy = report(&destination.verify(ACCOUNT), 0);
    assert_eq!(
        healthy.get("epoch").unwrap().as_str(),
        Some(StoreEpoch::from_bytes([9; 16]).to_string().as_str())
    );
    report(&destination.verify(OTHER), 1);
    report(&source.verify(ACCOUNT), 0);
    let broken = Fixture::new();
    let path = source.0.join("metadata.sqlite3");
    let mut bytes = fs::read(&path).unwrap();
    bytes[0] = 0;
    fs::write(path, bytes).unwrap();
    let failed = report_command(
        &restore_command(&source, &broken).output().unwrap(),
        1,
        "restore",
    );
    assert_eq!(failed.get("stage").unwrap().as_str(), Some("source-open"));
    assert_eq!(
        failed.get("progress").unwrap().as_str(),
        Some("unpublished")
    );
    assert!(!broken.0.join("metadata.sqlite3").exists());
}

#[test]
fn restore_refuses_bad_arguments_locks_and_root_policy_before_copying() {
    if private_case("restore_refuses_bad_arguments_locks_and_root_policy_before_copying") {
        return;
    }
    let source = Fixture::new();
    let destination = Fixture::new();
    for flags in [
        vec!["--root", "duplicate"],
        vec!["--destination", "duplicate"],
        vec!["--timeout-seconds", "0"],
        vec!["--timeout-seconds", "18446744073709551615"],
        vec!["--all"],
    ] {
        let failed = report_command(
            &restore_command(&source, &destination)
                .args(flags)
                .output()
                .unwrap(),
            2,
            "restore",
        );
        assert_eq!(
            failed.get("error").unwrap().as_str(),
            Some("invalid-arguments")
        );
        assert_eq!(
            failed.get("progress").unwrap().as_str(),
            Some("unpublished")
        );
        assert!(!source.0.join("LOCK").exists());
        assert!(!destination.0.join("LOCK").exists());
    }
    let failed = restore_command(&source, &destination)
        .arg(std::ffi::OsString::from_vec(vec![0xff]))
        .output()
        .unwrap();
    report_command(&failed, 2, "restore");
    assert!(!source.0.join("LOCK").exists());
    assert!(!destination.0.join("LOCK").exists());
    source.seed();
    for (root, stage) in [(&source, "source-lock"), (&destination, "destination-lock")] {
        let held = PrivateRoot::open(root.0.to_str().unwrap())
            .unwrap()
            .try_lock()
            .unwrap();
        let failed = report_command(
            &restore_command(&source, &destination).output().unwrap(),
            1,
            "restore",
        );
        assert_eq!(failed.get("stage").unwrap().as_str(), Some(stage));
        assert_eq!(failed.get("error").unwrap().as_str(), Some("busy"));
        assert_eq!(
            failed.get("progress").unwrap().as_str(),
            Some("unpublished")
        );
        assert!(!destination.0.join("metadata.sqlite3").exists());
        drop(held);
    }
    fs::set_permissions(&destination.0, fs::Permissions::from_mode(0o755)).unwrap();
    let failed = report_command(
        &restore_command(&source, &destination).output().unwrap(),
        1,
        "restore",
    );
    assert_eq!(
        failed.get("stage").unwrap().as_str(),
        Some("destination-root")
    );
    assert_eq!(
        failed.get("progress").unwrap().as_str(),
        Some("unpublished")
    );
    fs::set_permissions(&destination.0, fs::Permissions::from_mode(0o700)).unwrap();
    let before = fs::read(source.0.join("metadata.sqlite3")).unwrap();
    let failed = report_command(
        &restore_command(&source, &source).output().unwrap(),
        1,
        "restore",
    );
    assert_eq!(
        failed.get("progress").unwrap().as_str(),
        Some("unpublished")
    );
    assert_eq!(fs::read(source.0.join("metadata.sqlite3")).unwrap(), before);
    report(&source.verify(ACCOUNT), 0);
}

fn init_command(root: &Fixture, account: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_td-mta"));
    command
        .args(["store", "init", "--root"])
        .arg(&root.0)
        .args(["--account", account]);
    command
}

#[test]
fn init_creates_one_verifiable_account_inbox_and_never_replaces_it() {
    if private_case("init_creates_one_verifiable_account_inbox_and_never_replaces_it") {
        return;
    }
    let fixture = Fixture::new();
    let account = ACCOUNT.to_string();
    let output = init_command(&fixture, &account)
        .args(["--timeout-seconds", "60"])
        .output()
        .unwrap();
    let json = report_command(&output, 0, "store.init");
    assert_eq!(json.get("status").unwrap().as_str(), Some("ok"));
    assert_eq!(
        json.get("account").unwrap().as_str(),
        Some(account.as_str())
    );
    assert_eq!(json.get("sequence").unwrap().as_u64(), Some(1));
    assert_eq!(json.get("service_ready"), Some(&td_json::Json::Bool(false)));
    let epoch = StoreEpoch::parse(json.get("epoch").unwrap().as_str().unwrap()).unwrap();
    let inbox = MailboxId::parse(json.get("inbox").unwrap().as_str().unwrap()).unwrap();
    assert_ne!(epoch.as_bytes(), inbox.as_bytes());
    let database = fixture.0.join("metadata.sqlite3");
    assert_eq!(fs::metadata(&database).unwrap().mode() & 0o7777, 0o600);
    {
        let mut root = PrivateRoot::open(fixture.0.to_str().unwrap())
            .unwrap()
            .try_lock()
            .unwrap();
        let clock: Arc<dyn Clock> = Arc::new(RuntimeClock::new());
        let deadline = Deadline::after(clock.sample().unwrap().monotonic, 60_000).unwrap();
        let store = IndexStore::open(&mut root, clock, 1, deadline).unwrap();
        assert_eq!(store.epoch(), epoch);
        assert_eq!(store.account_ids(deadline).unwrap(), vec![ACCOUNT]);
        let mut view = store.view(ACCOUNT, deadline).unwrap();
        assert_eq!(view.identity().committed_sequence, Sequence::from_u64(1));
        let mut value = [0; 256];
        let (row, changed) = view.get(Key::Mailbox(inbox), &mut value).unwrap().unwrap();
        assert_eq!(changed, Sequence::from_u64(1));
        assert_eq!(
            row,
            Row::Mailbox(MailboxRow {
                name: "Inbox",
                parent: None,
                role: Some("inbox"),
                sort_order: 0,
                subscribed: true,
            })
        );
    }
    let all = report(
        &Command::new(env!("CARGO_BIN_EXE_td-mta"))
            .args(["store", "verify", "--root"])
            .arg(&fixture.0)
            .arg("--all")
            .output()
            .unwrap(),
        0,
    );
    assert_eq!(all.get("epoch"), json.get("epoch"));
    for (field, expected) in [("accounts", 1), ("mailboxes", 1), ("blobs", 0)] {
        assert_eq!(all.get(field).unwrap().as_u64(), Some(expected), "{field}");
    }
    let selected = report(&fixture.verify(ACCOUNT), 0);
    assert_eq!(selected.get("sequence").unwrap().as_u64(), Some(1));
    assert_eq!(selected.get("mailboxes").unwrap().as_u64(), Some(1));
    let before = fs::read(&database).unwrap();
    for other in [account.clone(), OTHER.to_string()] {
        let refused = report_command(
            &init_command(&fixture, &other).output().unwrap(),
            1,
            "store.init",
        );
        assert_eq!(refused.get("stage").unwrap().as_str(), Some("create"));
        assert_eq!(refused.get("error").unwrap().as_str(), Some("exists"));
        assert_eq!(refused.get("progress").unwrap().as_str(), Some("unstarted"));
        assert!(refused.get("epoch").is_none());
    }
    assert_eq!(fs::read(&database).unwrap(), before);
    let reverified = report(&fixture.verify(ACCOUNT), 0);
    assert_eq!(reverified.get("epoch"), json.get("epoch"));
    assert_eq!(
        report(&fixture.verify(OTHER), 1)
            .get("error")
            .unwrap()
            .as_str(),
        Some("not-found")
    );
}

#[test]
fn init_refuses_arguments_roots_and_locks_before_creation() {
    if private_case("init_refuses_arguments_roots_and_locks_before_creation") {
        return;
    }
    let fixture = Fixture::new();
    let account = ACCOUNT.to_string();
    let database = fixture.0.join("metadata.sqlite3");
    for extra in [
        vec!["--all"],
        vec!["--account", "duplicate"],
        vec!["--timeout-seconds", "0"],
        vec!["--timeout-seconds", "18446744073709551615"],
        vec!["--unknown", "x"],
    ] {
        let output = init_command(&fixture, &account)
            .args(extra)
            .output()
            .unwrap();
        let json = report_command(&output, 2, "store.init");
        assert_eq!(json.get("stage").unwrap().as_str(), Some("arguments"));
        assert_eq!(json.get("progress").unwrap().as_str(), Some("unstarted"));
        assert!(!fixture.0.join("LOCK").exists());
    }
    for args in [
        vec!["--root", "unused"],
        vec!["--root", "unused", "--all"],
        vec!["--root", "unused", "--account", "ABC"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_td-mta"))
            .args(["store", "init"])
            .args(args)
            .output()
            .unwrap();
        let json = report_command(&output, 2, "store.init");
        assert_eq!(json.get("stage").unwrap().as_str(), Some("arguments"));
        assert_eq!(
            json.get("error").unwrap().as_str(),
            Some("invalid-arguments")
        );
        assert_eq!(json.get("progress").unwrap().as_str(), Some("unstarted"));
    }
    let held = PrivateRoot::open(fixture.0.to_str().unwrap())
        .unwrap()
        .try_lock()
        .unwrap();
    let json = report_command(
        &init_command(&fixture, &account).output().unwrap(),
        1,
        "store.init",
    );
    assert_eq!(json.get("stage").unwrap().as_str(), Some("lock"));
    assert_eq!(json.get("error").unwrap().as_str(), Some("busy"));
    assert_eq!(json.get("progress").unwrap().as_str(), Some("unstarted"));
    drop(held);
    assert!(!database.exists());
    fs::set_permissions(&fixture.0, fs::Permissions::from_mode(0o755)).unwrap();
    let json = report_command(
        &init_command(&fixture, &account).output().unwrap(),
        1,
        "store.init",
    );
    assert_eq!(json.get("stage").unwrap().as_str(), Some("root"));
    assert_eq!(json.get("progress").unwrap().as_str(), Some("unstarted"));
    fs::set_permissions(&fixture.0, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!database.exists());
    let json = report_command(
        &init_command(&fixture, &account).output().unwrap(),
        0,
        "store.init",
    );
    assert_eq!(json.get("status").unwrap().as_str(), Some("ok"));
    assert!(database.exists());
}

#[path = "support/tls_certificate_fixture.rs"]
mod certificate_fixture;

struct ConfigFixture {
    data: Fixture,
    etc: Fixture,
}
impl ConfigFixture {
    fn new() -> Self {
        let fixture = Self {
            data: Fixture::new(),
            etc: Fixture::new(),
        };
        let provider = td_crypto::Provider;
        let mut raw = [0; td_crypto::P256_PKCS8_CAPACITY];
        let count = provider.generate_p256(&mut raw).unwrap();
        let root = provider.load_p256(&raw[..count]).unwrap();
        let ca = certificate_fixture::pem(
            "CERTIFICATE",
            &certificate_fixture::certificate_names(&root, &root, true, 1, &[], false),
        );
        let count = provider.generate_p256(&mut raw).unwrap();
        let leaf = provider.load_p256(&raw[..count]).unwrap();
        let chain = [
            certificate_fixture::pem(
                "CERTIFICATE",
                &certificate_fixture::certificate_names(
                    &leaf,
                    &root,
                    false,
                    2,
                    &["localhost"],
                    false,
                ),
            ),
            ca.clone(),
        ]
        .concat();
        let key = certificate_fixture::pem("PRIVATE KEY", &raw[..count]);
        for (name, bytes, mode) in [
            ("password", &b"relay-secret\n"[..], 0o600),
            ("relay-ca", &ca[..], 0o644),
            ("chain", &chain[..], 0o644),
            ("key", &key[..], 0o600),
            ("signature", &b"-- \nsignature\n"[..], 0o644),
        ] {
            fixture.write(name, bytes, mode);
        }
        fixture.configure("");
        fixture
    }
    fn path(&self, name: &str) -> PathBuf {
        self.etc.0.join(name)
    }
    fn write(&self, name: &str, bytes: &[u8], mode: u32) {
        let path = self.path(name);
        let _ = fs::remove_file(&path);
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    }
    fn mode(&self, name: &str, mode: u32) {
        fs::set_permissions(self.path(name), fs::Permissions::from_mode(mode)).unwrap();
    }
    fn configure(&self, extra: &str) {
        let etc = self.etc.0.to_str().unwrap();
        let source = format!(
            r#"version = 1
[server]
hostname = "localhost"
jmap_origin = "https://localhost:8443"
[paths]
data = "{data}"
[account "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
username = "private-user"
[domain "example.test"]
[alias "main@example.test"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[identity "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
email = "main@example.test"
text_signature_file = "{etc}/signature"
[resolver "primary"]
address = "127.0.0.1:53"
[relay]
host = "localhost"
port = 465
username = "private-relay"
password_file = "{etc}/password"
ca_file = "{etc}/relay-ca"
[certificate "public"]
mode = "files"
chain_file = "{etc}/chain"
key_file = "{etc}/key"
[listener "smtp"]
kind = "direct_smtp"
bind = "127.0.0.1:2525"
server_name = "localhost"
certificate = "public"
session_limit = 1
per_peer_limit = 1
[listener "https"]
kind = "https"
bind = "127.0.0.1:8443"
certificate = "public"
{extra}"#,
            data = self.data.0.to_str().unwrap(),
        );
        self.write("td-mta.conf", source.as_bytes(), 0o644);
    }
    fn check(&self, code: i32) -> td_json::Json {
        let output = Command::new(env!("CARGO_BIN_EXE_td-mta"))
            .args(["config", "check", "--config"])
            .arg(self.path("td-mta.conf"))
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!stdout.contains(self.etc.0.to_str().unwrap()), "{stdout}");
        assert!(!stdout.contains(self.data.0.to_str().unwrap()), "{stdout}");
        assert!(!stdout.contains("relay-secret"), "{stdout}");
        assert_eq!(output.status.code(), Some(code), "{stdout}");
        report_command(&output, code, "config.check")
    }
    fn refuses(&self, stage: &str, error: &str, target: Option<&str>) -> td_json::Json {
        let json = self.check(1);
        assert_eq!(json.get("status").unwrap().as_str(), Some("error"));
        assert_eq!(json.get("stage").unwrap().as_str(), Some(stage), "{json:?}");
        assert_eq!(json.get("error").unwrap().as_str(), Some(error), "{json:?}");
        assert_eq!(
            json.get("target").and_then(td_json::Json::as_str),
            target,
            "{json:?}"
        );
        json
    }
}

#[test]
fn config_check_opens_every_protected_input_and_separates_secrets() {
    if private_case("config_check_opens_every_protected_input_and_separates_secrets") {
        return;
    }
    let fixture = ConfigFixture::new();
    let json = fixture.check(0);
    assert_eq!(json.get("status").unwrap().as_str(), Some("ok"));
    // The configuration, signature, password, relay CA, chain and key; one
    // profile serves both listeners.
    assert_eq!(json.get("inputs").unwrap().as_u64(), Some(6));

    let input = Some("certificate-key");
    for (name, mode, error, target) in [
        ("key", 0o640, "input-private-mode", input),
        ("key", 0o400, "", input),
        ("key", 0o700, "input-mode", input),
        (
            "password",
            0o644,
            "input-private-mode",
            Some("relay-password"),
        ),
        ("chain", 0o664, "input-writable", Some("certificate-chain")),
        ("chain", 0o4644, "input-mode", Some("certificate-chain")),
        ("relay-ca", 0o646, "input-writable", Some("relay-ca")),
        ("signature", 0o744, "input-mode", Some("text-signature")),
    ] {
        let original = fs::metadata(fixture.path(name)).unwrap().mode() & 0o7777;
        fixture.mode(name, mode);
        if error.is_empty() {
            fixture.check(0);
        } else {
            fixture.refuses("input", error, target);
        }
        fixture.mode(name, original);
    }
    fixture.check(0);

    // A hard link makes the private key also a signature: refused by inode.
    fs::remove_file(fixture.path("signature")).unwrap();
    fs::hard_link(fixture.path("key"), fixture.path("signature")).unwrap();
    fixture.refuses("input", "input-secret-alias", input);
    fs::remove_file(fixture.path("signature")).unwrap();
    fixture.write("signature", b"plain", 0o644);

    let ca = fs::read(fixture.path("relay-ca")).unwrap();
    fs::rename(fixture.path("relay-ca"), fixture.path("relay-ca-real")).unwrap();
    std::os::unix::fs::symlink(fixture.path("relay-ca-real"), fixture.path("relay-ca")).unwrap();
    fixture.refuses("input", "input-type", Some("relay-ca"));
    fs::remove_file(fixture.path("relay-ca")).unwrap();
    fixture.write("relay-ca", &ca, 0o644);

    let key = fs::read(fixture.path("key")).unwrap();
    fs::remove_file(fixture.path("key")).unwrap();
    fixture.refuses("input", "input-not-found", input);
    fixture.write("key", &key[..key.len() / 2], 0o600);
    fixture.refuses("tls", "tls", input);
    fixture.write("key", &key, 0o000);
    fixture.refuses("input", "input-private-mode", input);
    fixture.mode("key", 0o200);
    fixture.refuses("input", "input-private-mode", input);
    fixture.write("key", &key, 0o600);

    fixture.write("password", b"", 0o600);
    fixture.refuses("input", "config_material_password", Some("relay-password"));
    fixture.write("password", b"relay-secret\n", 0o600);

    fixture.mode("td-mta.conf", 0o664);
    fixture.refuses("configuration-file", "input-writable", None);
    fixture.mode("td-mta.conf", 0o644);
    fs::set_permissions(&fixture.etc.0, fs::Permissions::from_mode(0o770)).unwrap();
    fixture.refuses("configuration-file", "input-writable", None);
    fs::set_permissions(&fixture.etc.0, fs::Permissions::from_mode(0o700)).unwrap();

    fs::set_permissions(&fixture.data.0, fs::Permissions::from_mode(0o750)).unwrap();
    fixture.refuses("data-root", "root-policy-or-io", None);
    fs::set_permissions(&fixture.data.0, fs::Permissions::from_mode(0o700)).unwrap();

    fixture.configure("[unknown]\n");
    let json = fixture.refuses("configuration", "config_schema", None);
    assert!(json
        .get("detail")
        .unwrap()
        .as_str()
        .unwrap()
        .contains("line 39"));
    fixture.configure("");
    fixture.check(0);
}

#[test]
fn config_check_refuses_malformed_arguments_before_file_access() {
    for args in [
        vec!["config"],
        vec!["config", "check"],
        vec!["config", "check", "--config"],
        vec!["config", "check", "--root", "/etc/td-mta.conf"],
        vec!["config", "check", "--config", "/a", "--config", "/b"],
        vec!["config", "show", "--config", "/a"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_td-mta"))
            .args(args)
            .output()
            .unwrap();
        let json = report_command(&output, 2, "config.check");
        assert_eq!(json.get("stage").unwrap().as_str(), Some("arguments"));
        assert_eq!(
            json.get("error").unwrap().as_str(),
            Some("invalid-arguments")
        );
    }
    let output = Command::new(env!("CARGO_BIN_EXE_td-mta"))
        .args(["config", "check", "--config", "relative.conf"])
        .output()
        .unwrap();
    let json = report_command(&output, 1, "config.check");
    assert_eq!(
        json.get("stage").unwrap().as_str(),
        Some("configuration-file")
    );
    assert_eq!(json.get("error").unwrap().as_str(), Some("input-path"));
}

struct ReceivingProcess {
    child: std::process::Child,
    reader: Option<std::thread::JoinHandle<()>>,
}
impl Drop for ReceivingProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}
impl ReceivingProcess {
    fn start(fixture: &ConfigFixture) -> Self {
        use std::io::BufRead;
        let mut child = Command::new(env!("CARGO_BIN_EXE_td-mta"))
            .args(["serve", "--smtp-only", "--config"])
            .arg(fixture.path("td-mta.conf"))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut line = String::new();
            std::io::BufReader::new(stdout)
                .read_line(&mut line)
                .unwrap();
            let _ = send.send(line);
        });
        let process = Self {
            child,
            reader: Some(reader),
        };
        let line = receive
            .recv_timeout(std::time::Duration::from_secs(15))
            .unwrap();
        assert!(line.contains("\"status\":\"ready\""), "{line}");
        assert!(line.contains("\"profile\":\"smtp-only\""), "{line}");
        process
    }
}
fn smtp_command(stream: &mut std::io::BufReader<std::net::TcpStream>, command: &[u8], code: &str) {
    use std::io::{BufRead, Write};
    stream.get_mut().write_all(command).unwrap();
    loop {
        let mut line = String::new();
        stream.read_line(&mut line).unwrap();
        assert!(line.starts_with(code), "{line:?}, expected {code}");
        if line.as_bytes().get(3) == Some(&b' ') {
            break;
        }
    }
}
#[test]
fn serve_receives_and_recovers_after_process_death() {
    if private_case("serve_receives_and_recovers_after_process_death") {
        return;
    }
    use std::io::{BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    let fixture = ConfigFixture::new();
    let runtime = Fixture::new();
    let logs = Fixture::new();
    fs::DirBuilder::new()
        .mode(0o700)
        .create(fixture.data.0.join("ingress"))
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let source = fs::read_to_string(fixture.path("td-mta.conf"))
        .unwrap()
        .replace("127.0.0.1:2525", &address.to_string())
        .replace(
            "[paths]",
            &format!(
                "[paths]\nruntime = \"{}\"\nlogs = \"{}\"",
                runtime.0.display(),
                logs.0.display()
            ),
        );
    fixture.write("td-mta.conf", source.as_bytes(), 0o644);
    let output = Command::new(env!("CARGO_BIN_EXE_td-mta"))
        .args(["store", "init", "--root"])
        .arg(&fixture.data.0)
        .args(["--account", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"])
        .output()
        .unwrap();
    report_command(&output, 0, "store.init");
    let refuses = |stage: &str, code: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_td-mta"))
            .args(["serve", "--smtp-only", "--config"])
            .arg(fixture.path("td-mta.conf"))
            .output()
            .unwrap();
        let report = report_command(&output, 1, "serve");
        assert_eq!(report.get("stage").unwrap().as_str(), Some(stage));
        assert_eq!(report.get("error").unwrap().as_str(), Some(code));
        assert!(!String::from_utf8_lossy(&output.stdout).contains(fixture.data.0.to_str().unwrap()));
    };
    fixture.write(
        "td-mta.conf",
        source
            .replace(runtime.0.to_str().unwrap(), &format!("/{}", "r".repeat(90)))
            .as_bytes(),
        0o644,
    );
    refuses("control", "socket-path-too-long");
    fixture.write("td-mta.conf", source.as_bytes(), 0o644);
    // Profile refusal precedes even attempting to take the occupied store lock.
    let root = PrivateRoot::open(fixture.data.0.to_str().unwrap())
        .unwrap()
        .try_lock()
        .unwrap();
    fixture.write(
        "td-mta.conf",
        source
            .replace(&address.to_string(), &format!("[::1]:{}", address.port()))
            .as_bytes(),
        0o644,
    );
    refuses("listeners", "unsupported-listener");
    drop(root);
    fixture.write("td-mta.conf", source.as_bytes(), 0o644);
    fs::remove_dir(fixture.data.0.join("ingress")).unwrap();
    refuses("spool-root", "root-policy-or-io");
    fs::DirBuilder::new()
        .mode(0o700)
        .create(fixture.data.0.join("ingress"))
        .unwrap();
    fixture.write(
        "td-mta.conf",
        source
            .replace(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "cccccccccccccccccccccccccccccccc",
            )
            .as_bytes(),
        0o644,
    );
    refuses("accounts", "configured-account-mismatch");
    fixture.write("td-mta.conf", source.as_bytes(), 0o644);
    refuses("listen", "address-in-use");
    drop(listener);
    // Refuse public roots without claiming readiness or opening a listener.
    fs::set_permissions(&logs.0, fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_td-mta"))
        .args(["serve", "--smtp-only", "--config"])
        .arg(fixture.path("td-mta.conf"))
        .output()
        .unwrap();
    let report = report_command(&output, 1, "serve");
    assert_eq!(report.get("stage").unwrap().as_str(), Some("log-root"));
    fs::set_permissions(&logs.0, fs::Permissions::from_mode(0o700)).unwrap();
    let socket = runtime.0.join("smtp-control.sock");
    fs::write(&socket, b"do not replace").unwrap();
    refuses("control", "socket-policy");
    assert_eq!(fs::read(&socket).unwrap(), b"do not replace");
    fs::remove_file(&socket).unwrap();
    std::os::unix::fs::symlink("missing", &socket).unwrap();
    refuses("control", "socket-policy");
    assert!(fs::symlink_metadata(&socket)
        .unwrap()
        .file_type()
        .is_symlink());
    fs::remove_file(&socket).unwrap();
    let process = ReceivingProcess::start(&fixture);
    assert_eq!(fs::metadata(&socket).unwrap().mode() & 0o7777, 0o600);
    let status = || {
        Command::new(env!("CARGO_BIN_EXE_td-mta"))
            .args(["status", "--json", "--runtime"])
            .arg(&runtime.0)
            .output()
            .unwrap()
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let report = report_command(&status(), 0, "status");
        if report.get("state").unwrap().as_str() == Some("ready") {
            break;
        }
        assert_eq!(report.get("state").unwrap().as_str(), Some("starting"));
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o644)).unwrap();
    let report = report_command(&status(), 1, "status");
    assert_eq!(report.get("error").unwrap().as_str(), Some("socket-policy"));
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_td-mta"))
        .args(["serve", "--smtp-only", "--config"])
        .arg(fixture.path("td-mta.conf"))
        .output()
        .unwrap();
    let report = report_command(&output, 1, "serve");
    assert_eq!(report.get("stage").unwrap().as_str(), Some("store-lock"));
    let connect = || {
        let stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut stream = BufReader::new(stream);
        smtp_command(&mut stream, b"", "220");
        smtp_command(&mut stream, b"EHLO sender.test\r\n", "250");
        stream
    };
    let mut stream = connect();
    smtp_command(&mut stream, b"MAIL FROM:<>\r\n", "250");
    smtp_command(&mut stream, b"RCPT TO:<other@outside.test>\r\n", "550");
    smtp_command(&mut stream, b"RCPT TO:<main@example.test>\r\n", "250");
    smtp_command(&mut stream, b"DATA\r\n", "354");
    smtp_command(
        &mut stream,
        b"Subject: process-recovery\r\n\r\naccepted-body\r\n.\r\n",
        "250",
    );
    // A second, unfinished DATA owns a spool slot when the process dies.
    smtp_command(&mut stream, b"MAIL FROM:<>\r\n", "250");
    smtp_command(&mut stream, b"RCPT TO:<main@example.test>\r\n", "250");
    smtp_command(&mut stream, b"DATA\r\n", "354");
    stream
        .get_mut()
        .write_all(b"Subject: unfinished\r\n\r\nunaccepted-body\r\n")
        .unwrap();
    assert!(fs::read_dir(fixture.data.0.join("ingress"))
        .unwrap()
        .any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("slot-")));
    drop(process);
    assert!(fs::symlink_metadata(&socket)
        .unwrap()
        .file_type()
        .is_socket());
    drop(stream);
    let mut process = ReceivingProcess::start(&fixture);
    assert_eq!(
        fs::read_dir(fixture.data.0.join("ingress"))
            .unwrap()
            .count(),
        1
    );
    let mut stream = connect();
    let output = Command::new(env!("CARGO_BIN_EXE_td-mta"))
        .args(["stop", "--runtime"])
        .arg(&runtime.0)
        .output()
        .unwrap();
    let report = report_command(&output, 0, "stop");
    assert_eq!(report.get("status").unwrap().as_str(), Some("requested"));
    smtp_command(&mut stream, b"", "421");
    drop(stream);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(status) = process.child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(!socket.exists());
    drop(process);
    let report = report_command(&status(), 1, "status");
    assert_eq!(report.get("error").unwrap().as_str(), Some("not-running"));
    // A same-owner bogus endpoint cannot make the client echo arbitrary data.
    let fake = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    fake.set_nonblocking(true).unwrap();
    let responder = std::thread::spawn(move || {
        use std::io::Read;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let (mut peer, _) = loop {
            match fake.accept() {
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(std::time::Instant::now() < deadline);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                result => break result.unwrap(),
            }
        };
        peer.set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .unwrap();
        let mut request = Vec::new();
        peer.read_to_end(&mut request).unwrap();
        assert_eq!(request, b"STATUS\n");
        peer.write_all(b"SENSITIVE\n").unwrap();
    });
    let output = status();
    responder.join().unwrap();
    let report = report_command(&output, 1, "status");
    assert_eq!(
        report.get("error").unwrap().as_str(),
        Some("invalid-response")
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("SENSITIVE"));
    fs::remove_file(&socket).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_td-mta"))
        .args(["store", "verify", "--root"])
        .arg(&fixture.data.0)
        .arg("--all")
        .output()
        .unwrap();
    let report = report_command(&output, 0, "store.verify");
    assert_eq!(report.get("blobs").unwrap().as_u64(), Some(1));
    // Inspect the reopened body through the typed store, never direct SQL.
    let mut root = PrivateRoot::open(fixture.data.0.to_str().unwrap())
        .unwrap()
        .try_lock()
        .unwrap();
    let clock: Arc<dyn Clock> = Arc::new(RuntimeClock::new());
    let deadline = Deadline::after(clock.sample().unwrap().monotonic, 60_000).unwrap();
    let store = IndexStore::open(&mut root, clock, 1, deadline).unwrap();
    let mut view = store
        .view(AccountId::from_bytes([0xaa; 16]), deadline)
        .unwrap();
    let mut key = [0; 128];
    let mut value = [0; 65536];
    let found = view
        .next(Table::Blobs, None, &mut key, &mut value)
        .unwrap()
        .unwrap();
    let (id, length) = match (found.key, found.row) {
        (Key::Blob(id), Row::Blob(row)) => Some((id, row.length)),
        _ => None,
    }
    .unwrap();
    let mut bytes = vec![0; length as usize];
    let mut body = view
        .open_blob_input(&td_crypto::Provider, id, 32768)
        .unwrap();
    let mut used = 0;
    while used < bytes.len() {
        let n = body.read(&mut bytes[used..]).unwrap();
        assert_ne!(n, 0);
        used += n;
    }
    body.finish().unwrap();
    assert!(bytes.ends_with(b"Subject: process-recovery\r\n\r\naccepted-body\r\n"));
}

#[test]
fn serve_requires_explicit_receiving_profile() {
    for args in [
        vec!["serve"],
        vec!["serve", "--config", "/absent"],
        vec!["serve", "--smtp-only"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_td-mta"))
            .args(args)
            .output()
            .unwrap();
        report_command(&output, 2, "serve");
    }
}

#[test]
fn control_requires_explicit_private_runtime_and_json_status() {
    for (command, args) in [
        ("status", vec!["status"]),
        ("status", vec!["status", "--runtime", "/absent"]),
        ("stop", vec!["stop"]),
        ("stop", vec!["stop", "--runtime", "/a", "extra"]),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_td-mta"))
            .args(args)
            .output()
            .unwrap();
        let report = report_command(&output, 2, command);
        assert_eq!(report.get("stage").unwrap().as_str(), Some("arguments"));
    }
}
