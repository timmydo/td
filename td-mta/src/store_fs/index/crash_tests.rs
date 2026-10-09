//! Real process death and stopped-database backup oracles.
#![allow(clippy::unwrap_used, clippy::panic)]
use super::super::tests::Fixture;
use super::super::{with_probe_root_path, BlobSource};
use super::*;
use crate::{
    format::row::{
        BlobKind, BlobRow, EmailOrigin, EmailRow, MailboxRow, NotificationState, SubmissionRow,
    },
    ids::{BlobId, EmailId, IdentityId, MailboxId, SubmissionId, ThreadId},
    ports::{BlobReader, Crypto, Digest, Tick, Time},
};
use std::{
    io::{self, Read, Write},
    os::unix::process::ExitStatusExt,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const ACCOUNT: AccountId = AccountId::from_bytes([0x71; 16]);
const BLOB: BlobId = BlobId::from_bytes([0x72; 16]);
const MAILBOX: MailboxId = MailboxId::from_bytes([0x73; 16]);
const EMAIL: EmailId = EmailId::from_bytes([0x74; 16]);
const THREAD: ThreadId = ThreadId::from_bytes([0x75; 16]);
const BODY_BYTES: u64 = 2 * 1024 * 1024;
const CHILD_CASE: &str = "store_fs::index::crash_tests::crash_child";
const BACKUP_CHILD_CASE: &str = "store_fs::index::crash_tests::backup_crash_child";
const DESTINATION_ENV: &str = "TD_MTA_BACKUP_CRASH_DESTINATION";
pub(super) const ROOT_ENV: &str = "TD_MTA_CRASH_FIXTURE_ROOT";
pub(super) const PHASE_ENV: &str = "TD_MTA_CRASH_FIXTURE_PHASE";

struct Fixed;
impl Clock for Fixed {
    fn sample(&self) -> Result<Time, ports::Error> {
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(1),
        })
    }
}
fn deadline() -> Deadline {
    Deadline::after(Tick(0), 100).unwrap()
}
fn request(sequence: u64) -> CommitRequest {
    CommitRequest {
        account: ACCOUNT,
        expected: Sequence::from_u64(sequence),
        utc_ms: 0,
        deadline: deadline(),
    }
}
fn encode(row: Row<'_>) -> Vec<u8> {
    let mut bytes = vec![0; 1024];
    let count = row.encode(&mut bytes).unwrap();
    bytes.truncate(count);
    bytes
}
fn blob_row() -> Vec<u8> {
    let mut digest = td_crypto::Provider.sha256().unwrap();
    let chunk = [0x5a; 4096];
    for _ in 0..BODY_BYTES / chunk.len() as u64 {
        digest.update(&chunk).unwrap();
    }
    encode(Row::Blob(BlobRow {
        kind: BlobKind::Message,
        length: BODY_BYTES,
        digest: digest.finish().unwrap(),
        created_at: 0,
    }))
}
fn mailbox_row() -> Vec<u8> {
    encode(Row::Mailbox(MailboxRow {
        name: "Recovered",
        parent: None,
        role: None,
        sort_order: 0,
        subscribed: true,
    }))
}
pub(super) fn acknowledge_and_wait(root: &Path, phase: &str) {
    let path = root.join("crash-ready");
    let mut marker = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    marker.write_all(phase.as_bytes()).unwrap();
    marker.sync_all().unwrap();
    fs::File::open(root).unwrap().sync_all().unwrap();
    // The parent retains this pipe until it kills and reaps this exact child.
    let mut byte = [0];
    io::stdin().read_exact(&mut byte).unwrap();
    panic!("crash child unexpectedly resumed");
}
struct Generated<'a> {
    remaining: u64,
    pause_root: Option<&'a Path>,
}
impl Read for Generated<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.remaining <= BODY_BYTES / 2 {
            if let Some(root) = self.pause_root.take() {
                // The child checkpointed before this transaction; these frames
                // prove the interrupted write actually reached SQLite's WAL.
                assert!(
                    fs::metadata(root.join("metadata.sqlite3-wal"))
                        .unwrap()
                        .len()
                        > 4096
                );
                acknowledge_and_wait(root, "before-commit");
            }
        }
        let count = output.len().min(64 * 1024).min(self.remaining as usize);
        output.get_mut(..count).unwrap().fill(0x5a);
        self.remaining -= count as u64;
        Ok(count)
    }
}

#[test]
#[ignore = "child endpoint invoked only by the abrupt-death parent oracle"]
fn crash_child() {
    let root_path = std::env::var_os(ROOT_ENV).unwrap();
    let root_path = Path::new(&root_path);
    let phase = std::env::var(PHASE_ENV).unwrap();
    assert!(matches!(phase.as_str(), "before-commit" | "after-commit"));
    with_probe_root_path(root_path, |root| {
        let store = IndexStore::open(root, Arc::new(Fixed), 1, deadline()).unwrap();
        store.checkpoint(deadline()).unwrap();
        let blob = blob_row();
        let mailbox = mailbox_row();
        let operations = [
            Operation::put(Table::Blobs, BLOB.as_bytes(), &blob).unwrap(),
            Operation::put(Table::Mailboxes, MAILBOX.as_bytes(), &mailbox).unwrap(),
        ];
        let mut source = Generated {
            remaining: BODY_BYTES,
            pause_root: (phase == "before-commit").then_some(root_path),
        };
        assert_eq!(
            store
                .commit(
                    &td_crypto::Provider,
                    request(0),
                    &operations,
                    &mut [BlobSource {
                        id: BLOB,
                        source: &mut source
                    }]
                )
                .unwrap(),
            Sequence::from_u64(1)
        );
        assert_eq!(phase, "after-commit");
        acknowledge_and_wait(root_path, "after-commit");
    });
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn kill_at(root: &Path, phase: &str) {
    kill_child_at(root, phase, CHILD_CASE, None);
}
pub(super) fn kill_child_at(root: &Path, phase: &str, case: &str, destination: Option<&Path>) {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            case,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ROOT_ENV, root)
        .env(PHASE_ENV, phase)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    match destination {
        Some(path) => {
            command.env(DESTINATION_ENV, path);
        }
        None => {
            command.env_remove(DESTINATION_ENV);
        }
    }
    let mut child = ChildGuard(command.spawn().unwrap());
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "crash child exited before reaching its boundary"
        );
        match fs::read(root.join("crash-ready")) {
            Ok(bytes) if bytes == phase.as_bytes() => break,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => panic!("read crash marker: {error}"),
        }
        assert!(
            Instant::now() < until,
            "crash child did not reach requested boundary"
        );
        thread::sleep(Duration::from_millis(5));
    }
    child.0.kill().unwrap();
    assert_eq!(child.0.wait().unwrap().signal(), Some(9));
}
fn verify_body(view: &mut IndexReadView<'_, '_>, id: BlobId) {
    let mut input = view
        .open_blob_input(&td_crypto::Provider, id, BODY_BYTES)
        .unwrap();
    let mut chunk = [0; 4096];
    while input.position() < BODY_BYTES {
        let count = input.read(&mut chunk).unwrap();
        assert!(count > 0);
        assert!(chunk.get(..count).unwrap().iter().all(|&byte| byte == 0x5a));
    }
    let mut pin = input.finish().unwrap();
    assert_eq!(pin.len(), BODY_BYTES);
    assert_eq!(pin.read_at(BODY_BYTES - 1, &mut chunk).unwrap(), 1);
    assert_eq!(chunk.first(), Some(&0x5a));
}
#[test]
fn abrupt_death_before_and_after_commit_recovers_atomic_body_and_metadata() {
    for phase in ["before-commit", "after-commit"] {
        let fixture = Fixture::new();
        {
            let mut root = fixture.locked();
            let store = IndexStore::create(
                &mut root,
                StoreEpoch::from_bytes([0x76; 16]),
                Arc::new(Fixed),
                1,
                deadline(),
            )
            .unwrap();
            store.create_account(ACCOUNT, deadline()).unwrap();
        }
        kill_at(&fixture.path, phase);
        let mut root = fixture.locked();
        let store = IndexStore::open(&mut root, Arc::new(Fixed), 1, deadline()).unwrap();
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        let mut bytes = [0; 1024];
        if phase == "before-commit" {
            assert_eq!(view.identity().committed_sequence, Sequence::default());
            assert!(view.get(Key::Blob(BLOB), &mut bytes).unwrap().is_none());
            assert!(view
                .get(Key::Mailbox(MAILBOX), &mut bytes)
                .unwrap()
                .is_none());
            drop(view);
            // Recovery must also undo the permanent-ID registration.
            let blob = blob_row();
            let mut source = Generated {
                remaining: BODY_BYTES,
                pause_root: None,
            };
            assert_eq!(
                store
                    .commit(
                        &td_crypto::Provider,
                        request(0),
                        &[Operation::put(Table::Blobs, BLOB.as_bytes(), &blob).unwrap()],
                        &mut [BlobSource {
                            id: BLOB,
                            source: &mut source
                        }]
                    )
                    .unwrap(),
                Sequence::from_u64(1)
            );
        } else {
            assert_eq!(view.identity().committed_sequence, Sequence::from_u64(1));
            assert!(matches!(
                view.get(Key::Mailbox(MAILBOX), &mut bytes).unwrap(),
                Some((
                    Row::Mailbox(MailboxRow {
                        name: "Recovered",
                        ..
                    }),
                    _
                ))
            ));
            verify_body(&mut view, BLOB);
        }
    }
}

#[test]
fn stopped_checkpoint_backup_restores_bodies_metadata_and_queue_retention() {
    let source = Fixture::new();
    let destination = Fixture::new();
    let queued_blob = BlobId::from_bytes([0x81; 16]);
    let deleted_email = EmailId::from_bytes([0x82; 16]);
    let submission = SubmissionId::from_bytes([0x83; 16]);
    let blob = blob_row();
    let mailbox = mailbox_row();
    let thread = encode(Row::Thread);
    let email = encode(Row::Email(EmailRow {
        blob: BLOB,
        thread: THREAD,
        received_at: 0,
        origin: EmailOrigin::Jmap,
    }));
    let queued_email = encode(Row::Email(EmailRow {
        blob: queued_blob,
        thread: THREAD,
        received_at: 0,
        origin: EmailOrigin::Jmap,
    }));
    let queued = encode(Row::Submission(SubmissionRow {
        email: deleted_email,
        thread: THREAD,
        identity: IdentityId::from_bytes([0x84; 16]),
        transmitted_blob: queued_blob,
        reverse_path: "sender@example.test",
        send_at: 0,
        expires_at: 1000,
        recipient_count: 1,
        completed_at: None,
        notification: NotificationState::None,
        notification_email: None,
    }));
    let membership = encode(Row::Membership);
    let recipient = encode(Row::Recipient(super::recipient_tests::queued()));
    let mut recipient_key = [0; 20];
    Key::Recipient(submission, 0)
        .encode(&mut recipient_key)
        .unwrap();
    let mut membership_key = [0; 32];
    Key::Membership(EMAIL, MAILBOX)
        .encode(&mut membership_key)
        .unwrap();
    {
        let mut root = source.locked();
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([0x85; 16]),
            Arc::new(Fixed),
            1,
            deadline(),
        )
        .unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        let operations = [
            Operation::put(Table::Blobs, BLOB.as_bytes(), &blob).unwrap(),
            Operation::put(Table::Blobs, queued_blob.as_bytes(), &blob).unwrap(),
            Operation::put(Table::Mailboxes, MAILBOX.as_bytes(), &mailbox).unwrap(),
            Operation::put(Table::Threads, THREAD.as_bytes(), &thread).unwrap(),
            Operation::put(Table::Emails, EMAIL.as_bytes(), &email).unwrap(),
            Operation::put(Table::Emails, deleted_email.as_bytes(), &queued_email).unwrap(),
            Operation::put(Table::Memberships, &membership_key, &membership).unwrap(),
            Operation::put(Table::Submissions, submission.as_bytes(), &queued).unwrap(),
            Operation::put(Table::Recipients, &recipient_key, &recipient).unwrap(),
        ];
        let mut first = Generated {
            remaining: BODY_BYTES,
            pause_root: None,
        };
        let mut second = Generated {
            remaining: BODY_BYTES,
            pause_root: None,
        };
        store
            .commit(
                &td_crypto::Provider,
                request(0),
                &operations,
                &mut [
                    BlobSource {
                        id: BLOB,
                        source: &mut first,
                    },
                    BlobSource {
                        id: queued_blob,
                        source: &mut second,
                    },
                ],
            )
            .unwrap();
        store
            .commit(
                &td_crypto::Provider,
                request(1),
                &[Operation::delete(Table::Emails, deleted_email.as_bytes()).unwrap()],
                &mut [],
            )
            .unwrap();
        let mut destination_root = destination.locked();
        let receipt = store
            .backup(&mut destination_root, deadline(), &mut [0; 65536])
            .unwrap();
        assert_eq!(receipt.epoch, StoreEpoch::from_bytes([0x85; 16]));
        assert_eq!(
            receipt.bytes,
            fs::metadata(destination.path.join("metadata.sqlite3"))
                .unwrap()
                .len()
        );
        assert!(!destination
            .path
            .join("metadata.sqlite3.backup-partial")
            .exists());
        // Both cooperative locks still belong to these callers after backup.
        assert!(matches!(source.lock(), Err(super::super::LockError::Busy)));
        assert!(matches!(
            destination.lock(),
            Err(super::super::LockError::Busy)
        ));
    }
    assert!(!destination.path.join("metadata.sqlite3-wal").exists());
    assert!(!destination.path.join("metadata.sqlite3-shm").exists());
    let mut root = destination.locked();
    let restored = IndexStore::open(&mut root, Arc::new(Fixed), 1, deadline()).unwrap();
    let mut view = restored.view(ACCOUNT, deadline()).unwrap();
    assert_eq!(view.identity().committed_sequence, Sequence::from_u64(2));
    let mut bytes = [0; 1024];
    assert!(matches!(
        view.get(Key::Mailbox(MAILBOX), &mut bytes).unwrap(),
        Some((
            Row::Mailbox(MailboxRow {
                name: "Recovered",
                ..
            }),
            _
        ))
    ));
    assert!(matches!(
        view.get(Key::Email(EMAIL), &mut bytes).unwrap(),
        Some((Row::Email(EmailRow { blob: BLOB, .. }), _))
    ));
    assert!(view
        .get(Key::Membership(EMAIL, MAILBOX), &mut bytes)
        .unwrap()
        .is_some());
    assert!(view
        .get(Key::Email(deleted_email), &mut bytes)
        .unwrap()
        .is_none());
    assert!(
        matches!(view.get(Key::Submission(submission), &mut bytes).unwrap(), Some((Row::Submission(row), _)) if row.transmitted_blob == queued_blob)
    );
    assert!(matches!(
        view.get(Key::Recipient(submission, 0), &mut bytes).unwrap(),
        Some((Row::Recipient(row), _)) if row == super::recipient_tests::queued()
    ));
    verify_body(&mut view, BLOB);
    verify_body(&mut view, queued_blob);
}

struct BackupClock {
    source: std::path::PathBuf,
    destination: std::path::PathBuf,
    pause: bool,
}
impl Clock for BackupClock {
    fn sample(&self) -> Result<Time, ports::Error> {
        if self.pause {
            let partial = self.destination.join("metadata.sqlite3.backup-partial");
            if let Ok(metadata) = fs::metadata(partial) {
                if metadata.len() >= 65536 {
                    let source_bytes = fs::metadata(self.source.join("metadata.sqlite3"))
                        .unwrap()
                        .len();
                    assert!(metadata.len() < source_bytes);
                    assert!(!self.destination.join("metadata.sqlite3").exists());
                    acknowledge_and_wait(&self.source, "during-backup-copy");
                }
            }
        }
        Fixed.sample()
    }
}

#[test]
#[ignore = "child endpoint invoked only by the backup abrupt-death parent oracle"]
fn backup_crash_child() {
    let source_path = std::env::var_os(ROOT_ENV).unwrap();
    let destination_path = std::env::var_os(DESTINATION_ENV).unwrap();
    let phase = std::env::var(PHASE_ENV).unwrap();
    assert!(matches!(
        phase.as_str(),
        "during-backup-copy" | "after-backup-receipt"
    ));
    with_probe_root_path(Path::new(&source_path), |source_root| {
        with_probe_root_path(Path::new(&destination_path), |destination_root| {
            let clock = Arc::new(BackupClock {
                source: source_path.clone().into(),
                destination: destination_path.clone().into(),
                pause: phase == "during-backup-copy",
            });
            let store = IndexStore::open(source_root, clock, 2, deadline()).unwrap();
            let receipt = store
                .backup(destination_root, deadline(), &mut [0; 65536])
                .unwrap();
            assert_eq!(phase, "after-backup-receipt");
            assert_eq!(receipt.epoch, StoreEpoch::from_bytes([0x95; 16]));
            assert_eq!(
                receipt.bytes,
                fs::metadata(Path::new(&source_path).join("metadata.sqlite3"))
                    .unwrap()
                    .len()
            );
            assert_eq!(
                receipt.bytes,
                fs::metadata(Path::new(&destination_path).join("metadata.sqlite3"))
                    .unwrap()
                    .len()
            );
            assert!(!Path::new(&destination_path)
                .join("metadata.sqlite3.backup-partial")
                .exists());
            acknowledge_and_wait(Path::new(&source_path), "after-backup-receipt");
        });
    });
}

fn verify_backup_account(store: &IndexStore<'_>) {
    store.validate_integrity(deadline()).unwrap();
    assert_eq!(store.epoch(), StoreEpoch::from_bytes([0x95; 16]));
    let mut view = store.maintenance_view(ACCOUNT, deadline()).unwrap();
    assert_eq!(view.identity().committed_sequence, Sequence::from_u64(1));
    let report = view
        .verify_account(
            &td_crypto::Provider,
            0,
            super::super::AccountCheckLimits {
                metadata: crate::metadata_sweep::Limits {
                    rows: 2,
                    parent_reads: 1,
                },
                bodies: super::super::BodyCheckLimits {
                    blobs: 1,
                    bytes: BODY_BYTES,
                },
            },
            &mut [0; 65536],
        )
        .unwrap();
    assert_eq!(report.identity(), view.identity());
    assert_eq!(report.metadata().references().rows(), 2);
    assert_eq!(report.metadata().mailboxes().mailboxes(), 1);
    assert_eq!(
        (report.bodies().blobs(), report.bodies().bytes()),
        (1, BODY_BYTES)
    );
    let mut bytes = [0; 1024];
    assert!(matches!(
        view.get(Key::Mailbox(MAILBOX), &mut bytes).unwrap(),
        Some((
            Row::Mailbox(MailboxRow {
                name: "Recovered",
                ..
            }),
            _
        ))
    ));
    verify_body(&mut view, BLOB);
}

#[test]
fn abrupt_death_during_backup_copy_and_after_receipt_preserves_source_and_phase() {
    for phase in ["during-backup-copy", "after-backup-receipt"] {
        let source = Fixture::new();
        let destination = Fixture::new();
        let bytes = {
            let mut root = source.locked();
            let store = IndexStore::create(
                &mut root,
                StoreEpoch::from_bytes([0x95; 16]),
                Arc::new(Fixed),
                2,
                deadline(),
            )
            .unwrap();
            store.create_account(ACCOUNT, deadline()).unwrap();
            let blob = blob_row();
            let mailbox = mailbox_row();
            let mut input = Generated {
                remaining: BODY_BYTES,
                pause_root: None,
            };
            store
                .commit(
                    &td_crypto::Provider,
                    request(0),
                    &[
                        Operation::put(Table::Blobs, BLOB.as_bytes(), &blob).unwrap(),
                        Operation::put(Table::Mailboxes, MAILBOX.as_bytes(), &mailbox).unwrap(),
                    ],
                    &mut [BlobSource {
                        id: BLOB,
                        source: &mut input,
                    }],
                )
                .unwrap();
            store.checkpoint(deadline()).unwrap();
            fs::metadata(source.path.join("metadata.sqlite3"))
                .unwrap()
                .len()
        };
        kill_child_at(
            &source.path,
            phase,
            BACKUP_CHILD_CASE,
            Some(&destination.path),
        );
        let mut source_root = source.locked();
        let store = IndexStore::open(&mut source_root, Arc::new(Fixed), 2, deadline()).unwrap();
        verify_backup_account(&store);
        let mut destination_root = destination.locked();
        let final_path = destination.path.join("metadata.sqlite3");
        let partial = destination.path.join("metadata.sqlite3.backup-partial");
        if phase == "during-backup-copy" {
            assert!(!final_path.exists());
            let metadata = fs::metadata(&partial).unwrap();
            assert!(metadata.len() >= 65536 && metadata.len() < bytes);
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
            assert_eq!(metadata.nlink(), 1);
            assert!(matches!(
                IndexStore::open(&mut destination_root, Arc::new(Fixed), 1, deadline()),
                Err(ports::Error::Io {
                    kind: io::ErrorKind::NotFound,
                    ..
                })
            ));
            assert_eq!(
                store.backup(&mut destination_root, deadline(), &mut [0; 65536]),
                Err(super::backup::BackupError::Unpublished(
                    ports::Error::Conflict
                ))
            );
            assert_eq!(fs::metadata(&partial).unwrap().len(), metadata.len());
            assert!(!final_path.exists());
            let source_again =
                IndexStore::open(&mut source_root, Arc::new(Fixed), 1, deadline()).unwrap();
            verify_backup_account(&source_again);
        } else {
            assert!(!partial.exists());
            let metadata = fs::metadata(&final_path).unwrap();
            assert_eq!(metadata.len(), bytes);
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
            assert_eq!(metadata.nlink(), 1);
            let restored =
                IndexStore::open(&mut destination_root, Arc::new(Fixed), 2, deadline()).unwrap();
            verify_backup_account(&restored);
        }
    }
}
