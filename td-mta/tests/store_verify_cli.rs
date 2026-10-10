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
