#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use super::*;
use crate::{
    format::{key::Key, row::*, ObjectType, Sequence},
    ids::*,
    ports::{self, ChangeCursor, ChangeStep, Record},
};

const BLOB: BlobId = BlobId::from_bytes([4; 16]);
const SUBMISSION: SubmissionId = SubmissionId::from_bytes([5; 16]);
fn mailbox(n: u8, parent: Option<u8>) -> Record<'static> {
    Record {
        key: Key::Mailbox(MailboxId::from_bytes([n; 16])),
        row: Row::Mailbox(MailboxRow {
            name: "folder",
            parent: parent.map(|n| MailboxId::from_bytes([n; 16])),
            role: None,
            sort_order: 0,
            subscribed: true,
        }),
        last_change: Sequence::from_u64(1),
    }
}
struct View {
    identity: ViewIdentity,
    rows: Vec<Record<'static>>,
    nexts: usize,
    gets: usize,
    move_next: bool,
    move_get: bool,
    error: bool,
    hide_mailboxes: bool,
}
impl View {
    fn new() -> Self {
        Self {
            identity: ViewIdentity {
                account: AccountId::from_bytes([1; 16]),
                epoch: StoreEpoch::from_bytes([2; 16]),
                committed_sequence: Sequence::from_u64(1),
                history_floor: Sequence::default(),
            },
            rows: vec![
                Record {
                    key: Key::Blob(BLOB),
                    row: Row::Blob(BlobRow {
                        length: 0,
                        digest: [0; 32],
                        created_at: 0,
                    }),
                    last_change: Sequence::from_u64(1),
                },
                mailbox(6, Some(7)),
                mailbox(7, None),
                Record {
                    key: Key::Submission(SUBMISSION),
                    row: Row::Submission(SubmissionRow {
                        email: EmailId::from_bytes([1; 16]),
                        thread: ThreadId::from_bytes([2; 16]),
                        identity: IdentityId::from_bytes([3; 16]),
                        transmitted_blob: BLOB,
                        reverse_path: "from@example.test",
                        send_at: 0,
                        expires_at: 200,
                        recipient_count: 1,
                        completed_at: None,
                        notification: NotificationState::None,
                        notification_email: None,
                    }),
                    last_change: Sequence::from_u64(1),
                },
                Record {
                    key: Key::Recipient(SUBMISSION, 0),
                    row: Row::Recipient(RecipientRow {
                        address: "to@example.test",
                        state: RecipientState::Queued,
                        uncertain: false,
                        attempt: None,
                        attempt_count: 0,
                        last_attempt_at: None,
                        phase: AttemptPhase::None,
                        next_attempt_at: Some(0),
                        rcpt_reply: None,
                        data_reply: None,
                        reason: FailureReason::None,
                        diagnostic: "",
                    }),
                    last_change: Sequence::from_u64(1),
                },
            ],
            nexts: 0,
            gets: 0,
            move_next: false,
            move_get: false,
            error: false,
            hide_mailboxes: false,
        }
    }
    fn fault(&mut self, moved: bool) -> Result<(), ports::Error> {
        if moved {
            self.identity.epoch = StoreEpoch::from_bytes([99; 16]);
        }
        if self.error {
            return Err(ports::Error::Capacity);
        }
        Ok(())
    }
}
fn encoded(key: Key<'_>) -> Vec<u8> {
    let mut bytes = vec![0; crate::format::MAX_KEY_BYTES];
    let n = key.encode(&mut bytes).unwrap();
    bytes.truncate(n);
    bytes
}
impl ReadView for View {
    fn identity(&self) -> ViewIdentity {
        self.identity
    }
    fn get<'a>(
        &mut self,
        key: Key<'_>,
        value: &'a mut [u8],
    ) -> Result<Option<(Row<'a>, Sequence)>, ports::Error> {
        self.gets += 1;
        self.fault(self.move_get)?;
        let Some(record) = self.rows.iter().find(|r| r.key == key) else {
            return Ok(None);
        };
        let n = record.row.encode(value).unwrap();
        Ok(Some((
            Row::decode(record.row.table(), &value[..n]).unwrap(),
            record.last_change,
        )))
    }
    fn next<'a>(
        &mut self,
        table: Table,
        after: Option<&[u8]>,
        key: &'a mut [u8],
        value: &'a mut [u8],
    ) -> Result<Option<Record<'a>>, ports::Error> {
        self.nexts += 1;
        self.fault(self.move_next)?;
        if self.hide_mailboxes && table == Table::Mailboxes {
            return Ok(None);
        }
        let record = self
            .rows
            .iter()
            .filter(|r| r.row.table() == table)
            .filter(|r| after.is_none_or(|a| Key::decode(table, a).unwrap().compare(r.key).is_lt()))
            .min_by(|a, b| a.key.compare(b.key));
        let Some(record) = record else {
            return Ok(None);
        };
        let k = record.key.encode(key).unwrap();
        let v = record.row.encode(value).unwrap();
        Ok(Some(Record {
            key: Key::decode(table, &key[..k]).unwrap(),
            row: Row::decode(table, &value[..v]).unwrap(),
            last_change: record.last_change,
        }))
    }
    fn next_change(&mut self, _: ChangeCursor, _: ObjectType) -> Result<ChangeStep, ports::Error> {
        Err(ports::Error::Invalid)
    }
}
fn sweep(view: &View, rows: u64, parent_reads: u64) -> Sweep {
    Sweep::new(view.identity(), 0, Limits { rows, parent_reads })
}
fn turn(sweep: &mut Sweep, view: &mut View) -> Result<Step, Error> {
    let (nexts, gets) = (view.nexts, view.gets);
    let result = sweep.advance(view, &mut [0; 2048], &mut [0; 65536]);
    assert!(view.nexts - nexts <= 1);
    assert!(view.gets - gets <= 2);
    result
}
fn run(sweep: &mut Sweep, view: &mut View) -> Result<(), Error> {
    for _ in 0..100 {
        if turn(sweep, view)? == Step::Complete {
            return Ok(());
        }
    }
    panic!("metadata checks failed to terminate")
}
#[test]
fn composes_all_passes_with_fixed_turn_work_and_original_receipts() {
    let mut view = View::new();
    let identity = view.identity();
    let mut checks = sweep(&view, 5, 3);
    assert_eq!(run(&mut checks, &mut view), Ok(()));
    let counts = (view.nexts, view.gets);
    assert_eq!(turn(&mut checks, &mut view), Ok(Step::Complete));
    assert_eq!(counts, (view.nexts, view.gets));
    let complete = checks.finish().unwrap();
    assert_eq!(complete.references().identity(), identity);
    assert_eq!(complete.mailboxes().identity(), identity);
    assert_eq!(complete.recipients().identity(), identity);
    assert_eq!(complete.references().rows(), 5);
    assert_eq!(complete.mailboxes().mailboxes(), 2);
    assert_eq!(complete.mailboxes().reads(), 3);
    assert_eq!(complete.recipients().submissions(), 1);
    assert_eq!(complete.recipients().recipients(), 1);
    assert_eq!(complete.references().utc_ms(), 0);
}
#[test]
fn refuses_unfinished_limits_missing_references_cycles_and_missing_recipients() {
    assert_eq!(sweep(&View::new(), 5, 3).finish(), Err(Error::Incomplete));
    for case in ["rows", "parents", "reference", "cycle", "recipient"] {
        let mut view = View::new();
        let mut checks = sweep(
            &view,
            if case == "rows" { 4 } else { 5 },
            if case == "parents" { 2 } else { 10 },
        );
        match case {
            "reference" => view.rows.retain(|r| r.key != Key::Blob(BLOB)),
            "cycle" => view.rows[2] = mailbox(7, Some(6)),
            "recipient" => view.rows.retain(|r| r.key != Key::Recipient(SUBMISSION, 0)),
            _ => (),
        }
        let error = run(&mut checks, &mut view).unwrap_err();
        match case {
            "rows" => assert_eq!(error, Error::References(reference_sweep::Error::RowLimit)),
            "parents" | "cycle" => assert!(matches!(error, Error::Mailboxes(_))),
            "reference" => assert!(matches!(error, Error::References(_))),
            "recipient" => assert!(matches!(
                error,
                Error::Recipients(recipient_sweep::Error::Missing { .. })
            )),
            _ => panic!(),
        }
        let calls = (view.nexts, view.gets);
        assert!(checks.is_failed());
        assert!(!checks.is_complete());
        assert_eq!(turn(&mut checks, &mut view), Err(Error::Failed));
        assert_eq!(calls, (view.nexts, view.gets));
        assert_eq!(checks.finish(), Err(Error::Failed));
    }
}
#[test]
fn refuses_view_movement_and_lookup_errors_in_every_pass() {
    for phase in 0..3 {
        for moved in [false, true] {
            let mut view = View::new();
            let mut checks = sweep(&view, 5, 3);
            for _ in 0..100 {
                let current = match checks.phase {
                    Phase::References(_) => 0,
                    Phase::Mailboxes(_) => 1,
                    Phase::Recipients(_) => 2,
                    _ => 3,
                };
                if current == phase {
                    break;
                }
                turn(&mut checks, &mut view).unwrap();
            }
            view.error = true;
            view.move_next = moved;
            let error = turn(&mut checks, &mut view).unwrap_err();
            if moved {
                assert_eq!(error, Error::ChangedView)
            } else {
                assert!(matches!(
                    error,
                    Error::References(reference_sweep::Error::View(ports::Error::Capacity))
                        | Error::Mailboxes(mailbox_sweep::Error::View(ports::Error::Capacity))
                        | Error::Recipients(recipient_sweep::Error::View(ports::Error::Capacity))
                ));
            }
            assert_eq!(checks.finish(), Err(Error::Failed));
        }
    }
    let mut view = View::new();
    let mut checks = sweep(&view, 5, 3);
    view.move_get = true;
    assert_eq!(run(&mut checks, &mut view), Err(Error::ChangedView));
}
#[test]
fn final_count_reconciliation_and_cached_identity_checks_cannot_be_skipped() {
    let mut view = View::new();
    let mut checks = sweep(&view, 5, 3);
    while matches!(checks.phase, Phase::References(_)) {
        turn(&mut checks, &mut view).unwrap();
    }
    view.hide_mailboxes = true;
    assert_eq!(run(&mut checks, &mut view), Err(Error::Counts));
    assert_eq!(checks.finish(), Err(Error::Failed));

    let mut view = View::new();
    let mut checks = sweep(&view, 5, 3);
    run(&mut checks, &mut view).unwrap();
    view.identity.epoch = StoreEpoch::from_bytes([99; 16]);
    let calls = (view.nexts, view.gets);
    assert_eq!(turn(&mut checks, &mut view), Err(Error::ChangedView));
    assert_eq!(calls, (view.nexts, view.gets));
    assert_eq!(checks.finish(), Err(Error::Failed));

    let mut view = View::new();
    let mut checks = sweep(&view, 5, 3);
    for _ in 0..100 {
        if matches!(checks.phase, Phase::Finalize) {
            break;
        }
        turn(&mut checks, &mut view).unwrap();
    }
    assert!(matches!(checks.phase, Phase::Finalize));
    assert!(!checks.is_complete());
    assert_eq!(checks.finish(), Err(Error::Incomplete));

    let mut view = View::new();
    view.rows.clear();
    let mut checks = sweep(&view, 0, 0);
    run(&mut checks, &mut view).unwrap();
    assert_eq!(checks.finish().unwrap().references().rows(), 0);
}

#[test]
fn all_passes_retain_one_native_wal_snapshot_while_the_writer_replaces_rows() {
    use crate::{
        format::operation::Operation,
        ports::{Clock, Deadline, Tick, Time},
        store_fs::{tests::Fixture, CommitRequest, IndexStore},
    };
    use std::sync::Arc;
    struct Fixed;
    impl Clock for Fixed {
        fn sample(&self) -> Result<Time, ports::Error> {
            Ok(Time {
                utc_ms: 0,
                monotonic: Tick(1),
            })
        }
    }
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let account = AccountId::from_bytes([1; 16]);
    let deadline = Deadline::after(Tick(0), 100).unwrap();
    let store = IndexStore::create(
        &mut root,
        StoreEpoch::from_bytes([2; 16]),
        Arc::new(Fixed),
        2,
        deadline,
    )
    .unwrap();
    store.create_account(account, deadline).unwrap();
    let mut old_value = [0; 128];
    let old = mailbox(6, None);
    let old_length = old.row.encode(&mut old_value).unwrap();
    let old_key = encoded(old.key);
    store
        .commit(
            &td_crypto::Provider,
            CommitRequest {
                account,
                epoch: store.epoch(),
                expected: Sequence::default(),
                deadline,
                utc_ms: 0,
            },
            &[Operation::put(Table::Mailboxes, &old_key, &old_value[..old_length]).unwrap()],
            &mut [],
        )
        .unwrap();
    let mut view = store.view(account, deadline).unwrap();
    let identity = view.identity();
    let mut checks = Sweep::new(
        identity,
        0,
        Limits {
            rows: 1,
            parent_reads: 1,
        },
    );
    let mut key = [0; 2048];
    let mut value = [0; 65536];
    checks.advance(&mut view, &mut key, &mut value).unwrap();
    let new = mailbox(7, None);
    let new_key = encoded(new.key);
    let mut new_value = [0; 128];
    let new_length = new.row.encode(&mut new_value).unwrap();
    store
        .commit(
            &td_crypto::Provider,
            CommitRequest {
                account,
                epoch: store.epoch(),
                expected: Sequence::from_u64(1),
                deadline,
                utc_ms: 0,
            },
            &[
                Operation::delete(Table::Mailboxes, &old_key).unwrap(),
                Operation::put(Table::Mailboxes, &new_key, &new_value[..new_length]).unwrap(),
            ],
            &mut [],
        )
        .unwrap();
    let mut rooted = Vec::new();
    for _ in 0..100 {
        match checks.advance(&mut view, &mut key, &mut value).unwrap() {
            Step::Mailboxes(mailbox_sweep::Step::Rooted { mailbox, .. }) => rooted.push(mailbox),
            Step::Complete => break,
            _ => (),
        }
    }
    assert_eq!(rooted, vec![MailboxId::from_bytes([6; 16])]);
    let complete = checks.finish().unwrap();
    assert_eq!(complete.references().identity(), identity);
    assert_eq!(complete.references().rows(), 1);
    assert_eq!(complete.mailboxes().reads(), 1);
    drop(view);
    let mut fresh = store.view(account, deadline).unwrap();
    assert_eq!(fresh.identity().committed_sequence, Sequence::from_u64(2));
    assert!(fresh.get(old.key, &mut value).unwrap().is_none());
    assert!(fresh.get(new.key, &mut value).unwrap().is_some());
}

struct ChecksClock;
impl ports::Clock for ChecksClock {
    fn sample(&self) -> Result<ports::Time, ports::Error> {
        Ok(ports::Time {
            utc_ms: 0,
            monotonic: ports::Tick(1),
        })
    }
}
fn complete_native_metadata<V: ReadView>(view: &mut V) -> CompleteMetadata {
    let mut checks = Sweep::new(
        view.identity(),
        0,
        Limits {
            rows: 4,
            parent_reads: 4,
        },
    );
    let mut key = [0; 2048];
    let mut value = [0; 65536];
    for _ in 0..100 {
        if checks.advance(view, &mut key, &mut value).unwrap() == Step::Complete {
            break;
        }
    }
    checks.finish().unwrap()
}

#[test]
fn account_reports_bind_all_identity_fields_and_declared_blob_counts() {
    use crate::{
        account_checks,
        format::operation::Operation,
        ports::{Deadline, Tick},
        store_fs::{
            tests::Fixture, BodyCheckLimits, CommitRequest, IndexStore, MAX_FILE_STEP_BYTES,
        },
    };
    use std::sync::Arc;
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let account = AccountId::from_bytes([1; 16]);
    let deadline = Deadline::after(Tick(0), 100).unwrap();
    let store = IndexStore::create(
        &mut root,
        StoreEpoch::from_bytes([2; 16]),
        Arc::new(ChecksClock),
        1,
        deadline,
    )
    .unwrap();
    store.create_account(account, deadline).unwrap();
    let record = mailbox(6, None);
    let mut bytes = [0; 128];
    let length = record.row.encode(&mut bytes).unwrap();
    let key = encoded(record.key);
    store
        .commit(
            &td_crypto::Provider,
            CommitRequest {
                account,
                epoch: store.epoch(),
                expected: Sequence::default(),
                deadline,
                utc_ms: 0,
            },
            &[Operation::put(Table::Mailboxes, &key, &bytes[..length]).unwrap()],
            &mut [],
        )
        .unwrap();
    store
        .commit(
            &td_crypto::Provider,
            CommitRequest {
                account,
                epoch: store.epoch(),
                expected: Sequence::from_u64(1),
                deadline,
                utc_ms: 0,
            },
            &[Operation::delete(Table::Mailboxes, &key).unwrap()],
            &mut [],
        )
        .unwrap();
    let mut native = store.view(account, deadline).unwrap();
    let identity = native.identity();
    assert_eq!(identity.committed_sequence, Sequence::from_u64(2));
    let bodies = native
        .verify_bodies(
            &td_crypto::Provider,
            BodyCheckLimits { blobs: 0, bytes: 0 },
            &mut [0; MAX_FILE_STEP_BYTES],
        )
        .unwrap();
    drop(native);
    for field in ["none", "account", "epoch", "sequence", "floor"] {
        let mut view = View::new();
        view.rows.clear();
        view.identity = identity;
        match field {
            "account" => view.identity.account = AccountId::from_bytes([11; 16]),
            "epoch" => view.identity.epoch = StoreEpoch::from_bytes([99; 16]),
            "sequence" => view.identity.committed_sequence = Sequence::from_u64(3),
            "floor" => view.identity.history_floor = Sequence::from_u64(1),
            _ => (),
        }
        let mut checks = sweep(&view, 0, 0);
        run(&mut checks, &mut view).unwrap();
        let calls = (view.nexts, view.gets);
        let result = account_checks::combine(checks.finish().unwrap(), bodies);
        assert_eq!((view.nexts, view.gets), calls);
        if field == "none" {
            let report = result.unwrap();
            assert_eq!(report.identity(), identity);
            assert_eq!(report.bodies(), bodies);
            assert_eq!(report.metadata().references().rows(), 0);
        } else {
            assert_eq!(result, Err(account_checks::Error::Identity), "{field}");
        }
    }
    // A same-identity supplied metadata scan with one extra declared blob.
    let mut view = View::new();
    view.identity = identity;
    view.rows
        .retain(|record| matches!(record.key, Key::Blob(_)));
    let mut checks = sweep(&view, 1, 0);
    run(&mut checks, &mut view).unwrap();
    assert_eq!(
        account_checks::combine(checks.finish().unwrap(), bodies),
        Err(account_checks::Error::BlobCount)
    );
}

#[test]
fn account_reports_preserve_matching_old_wal_results_and_refuse_cross_snapshot_pairs() {
    use crate::{
        account_checks,
        format::operation::Operation,
        ports::{Crypto, Deadline, Digest, Tick},
        store_fs::{
            tests::Fixture, BlobSource, BodyCheckLimits, CommitRequest, IndexStore,
            MAX_FILE_STEP_BYTES,
        },
    };
    use std::sync::Arc;
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let account = AccountId::from_bytes([1; 16]);
    let deadline = Deadline::after(Tick(0), 100).unwrap();
    let store = IndexStore::create(
        &mut root,
        StoreEpoch::from_bytes([2; 16]),
        Arc::new(ChecksClock),
        2,
        deadline,
    )
    .unwrap();
    store.create_account(account, deadline).unwrap();
    let row = |bytes: &[u8]| {
        let mut digest = td_crypto::Provider.sha256().unwrap();
        digest.update(bytes).unwrap();
        Row::Blob(BlobRow {
            length: bytes.len() as u64,
            digest: digest.finish().unwrap(),
            created_at: 0,
        })
    };
    let mut old_bytes = [0; 64];
    let length = row(b"abc").encode(&mut old_bytes).unwrap();
    let mut source = b"abc".as_slice();
    store
        .commit(
            &td_crypto::Provider,
            CommitRequest {
                account,
                epoch: store.epoch(),
                expected: Sequence::default(),
                deadline,
                utc_ms: 0,
            },
            &[Operation::put(Table::Blobs, BLOB.as_bytes(), &old_bytes[..length]).unwrap()],
            &mut [BlobSource {
                id: BLOB,
                source: &mut source,
            }],
        )
        .unwrap();
    let mut old = store.view(account, deadline).unwrap();
    let old_metadata = complete_native_metadata(&mut old);
    let replacement = BlobId::from_bytes([5; 16]);
    let mut bytes = [0; 64];
    let length = row(b"xy").encode(&mut bytes).unwrap();
    let mut source = b"xy".as_slice();
    store
        .commit(
            &td_crypto::Provider,
            CommitRequest {
                account,
                epoch: store.epoch(),
                expected: Sequence::from_u64(1),
                deadline,
                utc_ms: 0,
            },
            &[
                Operation::delete(Table::Blobs, BLOB.as_bytes()).unwrap(),
                Operation::put(Table::Blobs, replacement.as_bytes(), &bytes[..length]).unwrap(),
            ],
            &mut [BlobSource {
                id: replacement,
                source: &mut source,
            }],
        )
        .unwrap();
    let limits = BodyCheckLimits { blobs: 1, bytes: 3 };
    let mut scratch = [0; MAX_FILE_STEP_BYTES];
    let old_bodies = old
        .verify_bodies(&td_crypto::Provider, limits, &mut scratch)
        .unwrap();
    drop(old);
    let old_report = account_checks::combine(old_metadata, old_bodies).unwrap();
    assert_eq!(
        (old_report.bodies().blobs(), old_report.bodies().bytes()),
        (1, 3)
    );
    assert_eq!(
        old_report.identity().committed_sequence,
        Sequence::from_u64(1)
    );
    let mut fresh = store.view(account, deadline).unwrap();
    let fresh_metadata = complete_native_metadata(&mut fresh);
    let fresh_bodies = fresh
        .verify_bodies(&td_crypto::Provider, limits, &mut scratch)
        .unwrap();
    drop(fresh);
    let fresh_report = account_checks::combine(fresh_metadata, fresh_bodies).unwrap();
    assert_eq!(
        (fresh_report.bodies().blobs(), fresh_report.bodies().bytes()),
        (1, 2)
    );
    assert_eq!(
        fresh_report.identity().committed_sequence,
        Sequence::from_u64(2)
    );
    assert_eq!(
        old_metadata.references().table_rows(Table::Blobs),
        fresh_metadata.references().table_rows(Table::Blobs)
    );
    assert_eq!(
        account_checks::combine(old_metadata, fresh_bodies),
        Err(account_checks::Error::Identity)
    );
    assert_eq!(
        account_checks::combine(fresh_metadata, old_bodies),
        Err(account_checks::Error::Identity)
    );
}
