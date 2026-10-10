#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::{
    fs,
    os::unix::{
        ffi::OsStringExt,
        fs::{DirBuilderExt, MetadataExt, PermissionsExt},
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
        operation::Operation,
        row::{BlobKind, BlobRow, Row},
        Sequence, Table,
    },
    ids::{AccountId, BlobId, StoreEpoch},
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
                kind: BlobKind::Message,
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
