//! Final queue groups are checked inside the body/metadata transaction.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::super::tests::Fixture;
use super::*;
use crate::{
    format::row::{
        AttemptPhase, BlobKind, BlobRow, FailureReason, NotificationState, RecipientRow,
        RecipientState, SubmissionRow,
    },
    ids::{AttemptId, EmailId, IdentityId, SubmissionId, ThreadId},
    ports::{Crypto, Tick, Time},
};

const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
const BLOB: BlobId = BlobId::from_bytes([2; 16]);
const SUBMISSION: SubmissionId = SubmissionId::from_bytes([3; 16]);

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
fn request(expected: u64) -> CommitRequest {
    CommitRequest {
        account: ACCOUNT,
        expected: Sequence::from_u64(expected),
        utc_ms: 0,
        deadline: deadline(),
    }
}
fn open(root: &mut LockedRoot) -> IndexStore<'_> {
    let store = IndexStore::create(
        root,
        StoreEpoch::from_bytes([4; 16]),
        Arc::new(Fixed),
        2,
        deadline(),
    )
    .unwrap();
    store.create_account(ACCOUNT, deadline()).unwrap();
    store
}
fn encode(row: Row<'_>) -> Vec<u8> {
    let mut bytes = vec![0; 65536];
    let n = row.encode(&mut bytes).unwrap();
    bytes.truncate(n);
    bytes
}
fn submission(count: u32) -> SubmissionRow<'static> {
    SubmissionRow {
        email: EmailId::from_bytes([5; 16]),
        thread: ThreadId::from_bytes([6; 16]),
        identity: IdentityId::from_bytes([7; 16]),
        transmitted_blob: BLOB,
        reverse_path: "sender@example.test",
        send_at: 0,
        expires_at: 432_000_000,
        recipient_count: count,
        completed_at: None,
        notification: NotificationState::None,
        notification_email: None,
    }
}
pub(super) fn queued() -> RecipientRow<'static> {
    RecipientRow {
        address: "recipient@example.test",
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
    }
}
fn recipient_key(ordinal: u32) -> [u8; 20] {
    let mut key = [0; 20];
    Key::Recipient(SUBMISSION, ordinal)
        .encode(&mut key)
        .unwrap();
    key
}
fn create(store: &IndexStore<'_>, count: u32, ordinals: &[u32]) -> Result<Sequence, CommitError> {
    create_with(store, count, ordinals, false)
}
fn create_with(
    store: &IndexStore<'_>,
    count: u32,
    ordinals: &[u32],
    encoded: bool,
) -> Result<Sequence, CommitError> {
    create_with_recipient(store, count, ordinals, queued(), encoded)
}
fn create_with_recipient(
    store: &IndexStore<'_>,
    count: u32,
    ordinals: &[u32],
    recipient: RecipientRow<'_>,
    encoded: bool,
) -> Result<Sequence, CommitError> {
    let recipients: Vec<_> = ordinals
        .iter()
        .map(|&ordinal| (ordinal, recipient))
        .collect();
    create_group(store, submission(count), &recipients, encoded)
}
fn create_group(
    store: &IndexStore<'_>,
    submission: SubmissionRow<'_>,
    recipients: &[(u32, RecipientRow<'_>)],
    encoded: bool,
) -> Result<Sequence, CommitError> {
    let body = b"a prepared message";
    let mut digest = td_crypto::Provider.sha256().unwrap();
    digest.update(body).unwrap();
    let blob = encode(Row::Blob(BlobRow {
        kind: BlobKind::Message,
        length: body.len() as u64,
        digest: digest.finish().unwrap(),
        created_at: 0,
    }));
    let sub = encode(Row::Submission(submission));
    let rows: Vec<_> = recipients
        .iter()
        .map(|(_, row)| encode(Row::Recipient(*row)))
        .collect();
    let keys: Vec<_> = recipients
        .iter()
        .map(|(ordinal, _)| recipient_key(*ordinal))
        .collect();
    let mut operations = vec![
        Operation::put(Table::Blobs, BLOB.as_bytes(), &blob).unwrap(),
        Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &sub).unwrap(),
        Operation::change(
            ObjectType::EmailSubmission,
            ChangeAction::Created,
            SUBMISSION.as_bytes(),
        ),
    ];
    operations.extend(
        keys.iter()
            .zip(&rows)
            .map(|(key, row)| Operation::put(Table::Recipients, key, row).unwrap()),
    );
    let mut body = body.as_slice();
    let mut sources = [BlobSource {
        id: BLOB,
        source: &mut body,
    }];
    if encoded {
        commit_encoded(store, request(0), &operations, &mut sources)
    } else {
        store.commit(&td_crypto::Provider, request(0), &operations, &mut sources)
    }
}
fn rejected(result: Result<Sequence, CommitError>) {
    assert_eq!(result, Err(CommitError::Rejected(ports::Error::Conflict)));
}

#[test]
fn missing_recipients_roll_back_streamed_body_registry_metadata_and_changes() {
    for ordinals in [&[][..], &[0][..], &[1][..]] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = open(&mut root);
        rejected(create(&store, 2, ordinals));
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        assert_eq!(view.identity().committed_sequence, Sequence::default());
        for key in [
            Key::Blob(BLOB),
            Key::Submission(SUBMISSION),
            Key::Recipient(SUBMISSION, 0),
        ] {
            assert!(view.get(key, &mut [0; 1024]).unwrap().is_none());
        }
        assert_eq!(
            view.next_change(
                ChangeCursor {
                    sequence: Sequence::default(),
                    operation: 0
                },
                ObjectType::EmailSubmission
            )
            .unwrap(),
            ChangeStep::Complete
        );
        drop(view);
        // Reusing the same blob ID proves its permanent registry insert rolled back too.
        assert_eq!(create(&store, 2, &[0, 1]), Ok(Sequence::from_u64(1)));
        store.validate_integrity(deadline()).unwrap();
    }
}

#[test]
fn immutable_recipient_count_and_whole_group_deletion_check_final_rows() {
    for parent_first in [false, true] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = open(&mut root);
        create(&store, 2, &[0, 1]).unwrap();
        let key0 = recipient_key(0);
        let key1 = recipient_key(1);
        let remove0 = Operation::delete(Table::Recipients, &key0).unwrap();
        let remove1 = Operation::delete(Table::Recipients, &key1).unwrap();
        let reduced = encode(Row::Submission(submission(1)));
        let shrink = Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &reduced).unwrap();
        for operations in [&[remove0][..], &[remove1][..], &[shrink][..]] {
            rejected(store.commit(&td_crypto::Provider, request(1), operations, &mut []));
        }
        let old = store.view(ACCOUNT, deadline()).unwrap();
        rejected(store.commit(
            &td_crypto::Provider,
            request(1),
            &[shrink, remove1],
            &mut [],
        ));
        let parent = Operation::delete(Table::Submissions, SUBMISSION.as_bytes()).unwrap();
        rejected(store.commit(&td_crypto::Provider, request(1), &[parent], &mut []));
        let mut retained = store.view(ACCOUNT, deadline()).unwrap();
        assert_eq!(
            retained.identity().committed_sequence,
            Sequence::from_u64(1)
        );
        assert!(retained
            .get(Key::Recipient(SUBMISSION, 0), &mut [0; 1024])
            .unwrap()
            .is_some());
        drop(retained);
        let delete = if parent_first {
            [parent, remove0, remove1]
        } else {
            [remove0, remove1, parent]
        };
        assert_eq!(
            store.commit(&td_crypto::Provider, request(1), &delete, &mut []),
            Ok(Sequence::from_u64(2))
        );
        let mut old = old;
        assert!(old
            .get(Key::Recipient(SUBMISSION, 1), &mut [0; 1024])
            .unwrap()
            .is_some());
        let mut now = store.view(ACCOUNT, deadline()).unwrap();
        assert!(now
            .get(Key::Submission(SUBMISSION), &mut [0; 1024])
            .unwrap()
            .is_none());
    }
}

#[test]
fn final_state_and_aggregate_queue_rules_are_atomic() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store = open(&mut root);
    create(&store, 2, &[0, 1]).unwrap();
    let key0 = recipient_key(0);
    let key1 = recipient_key(1);
    let queued = queued();
    let mut invalid = queued;
    invalid.next_attempt_at = None;
    let bad = encode(Row::Recipient(invalid));
    let good = encode(Row::Recipient(queued));
    let bad_op = Operation::put(Table::Recipients, &key0, &bad).unwrap();
    let good_op = Operation::put(Table::Recipients, &key0, &good).unwrap();
    rejected(store.commit(
        &td_crypto::Provider,
        request(1),
        &[good_op, bad_op],
        &mut [],
    ));
    assert_eq!(
        store.commit(
            &td_crypto::Provider,
            request(1),
            &[bad_op, good_op],
            &mut []
        ),
        Ok(Sequence::from_u64(2))
    );

    let mut canceled = queued;
    canceled.state = RecipientState::Canceled;
    canceled.next_attempt_at = None;
    canceled.reason = FailureReason::Canceled;
    let canceled = encode(Row::Recipient(canceled));
    let cancel0 = Operation::put(Table::Recipients, &key0, &canceled).unwrap();
    let cancel1 = Operation::put(Table::Recipients, &key1, &canceled).unwrap();
    let mut completed = submission(2);
    completed.completed_at = Some(1);
    let complete = encode(Row::Submission(completed));
    let complete_op = Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &complete).unwrap();
    for operations in [
        &[cancel0][..],
        &[cancel0, complete_op][..],
        &[cancel0, cancel1][..],
        &[complete_op][..],
    ] {
        rejected(store.commit(&td_crypto::Provider, request(2), operations, &mut []));
    }
    assert_eq!(
        store.commit(
            &td_crypto::Provider,
            request(2),
            &[cancel0, cancel1, complete_op],
            &mut []
        ),
        Ok(Sequence::from_u64(3))
    );
}

#[test]
fn accepted_reply_and_failure_notice_rules_reuse_the_queue_validator() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store = open(&mut root);
    let key = recipient_key(0);
    let mut accepted = queued();
    accepted.state = RecipientState::Accepted;
    accepted.attempt = Some(AttemptId::from_bytes([8; 16]));
    accepted.attempt_count = 1;
    accepted.last_attempt_at = Some(1);
    accepted.phase = AttemptPhase::Final;
    accepted.next_attempt_at = None;
    accepted.rcpt_reply = Some("250 recipient accepted");
    accepted.data_reply = Some("354 send body");
    let mut in_flight = accepted;
    in_flight.state = RecipientState::InFlight;
    in_flight.phase = AttemptPhase::AcceptancePossible;
    in_flight.data_reply = None;
    create_with_recipient(&store, 1, &[0], in_flight, false).unwrap();
    let mut sub = submission(1);
    sub.completed_at = Some(2);
    let completed = encode(Row::Submission(sub));
    let complete = Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &completed).unwrap();
    let row = encode(Row::Recipient(accepted));
    rejected(store.commit(
        &td_crypto::Provider,
        request(1),
        &[
            complete,
            Operation::put(Table::Recipients, &key, &row).unwrap(),
        ],
        &mut [],
    ));
    accepted.data_reply = Some("250 stored");
    let row = encode(Row::Recipient(accepted));
    assert_eq!(
        store.commit(
            &td_crypto::Provider,
            request(1),
            &[
                complete,
                Operation::put(Table::Recipients, &key, &row).unwrap()
            ],
            &mut []
        ),
        Ok(Sequence::from_u64(2))
    );

    // Use a separate unattempted group for the failure-notice rule.
    let failure_fixture = Fixture::new();
    let mut failure_root = failure_fixture.locked();
    let store = open(&mut failure_root);
    create(&store, 1, &[0]).unwrap();
    let mut failed = queued();
    failed.state = RecipientState::Failed;
    failed.reason = FailureReason::Expired;
    failed.next_attempt_at = None;
    let row = encode(Row::Recipient(failed));
    let failure = Operation::put(Table::Recipients, &key, &row).unwrap();
    rejected(store.commit(&td_crypto::Provider, request(1), &[failure], &mut []));
    sub.notification = NotificationState::Pending;
    let noticed = encode(Row::Submission(sub));
    assert_eq!(
        store.commit(
            &td_crypto::Provider,
            request(1),
            &[
                failure,
                Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &noticed).unwrap()
            ],
            &mut []
        ),
        Ok(Sequence::from_u64(2))
    );
}

#[test]
fn maximum_group_fits_one_original_vm_budget_and_duplicate_mutations_validate_once() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store = open(&mut root);
    create(&store, 1000, &(0..1000).collect::<Vec<_>>()).unwrap();
    let consumed = {
        let writer = lock(&store.writer).unwrap();
        let remaining = lock(&writer.native.budget).unwrap().remaining;
        VM_STEPS - remaining
    };
    assert!(
        consumed < VM_STEPS / 2,
        "maximum group consumed {consumed} VM steps"
    );
    let key = recipient_key(999);
    let row = encode(Row::Recipient(queued()));
    let operation = Operation::put(Table::Recipients, &key, &row).unwrap();
    let repeated = vec![operation; 1000];
    assert_eq!(
        store.commit(&td_crypto::Provider, request(1), &repeated, &mut []),
        Ok(Sequence::from_u64(2))
    );
    store.validate_integrity(deadline()).unwrap();
}

#[test]
fn validation_seeks_only_the_affected_account_and_group() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store = open(&mut root);
    create(&store, 1, &[0]).unwrap();
    let other = AccountId::from_bytes([9; 16]);
    store.create_account(other, deadline()).unwrap();
    {
        let writer = lock(&store.writer).unwrap();
        let db = lock(&writer.native.connection).unwrap();
        // Deliberately incomplete unrelated groups expose an accidental full sweep.
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        for n in 0..2000u32 {
            let mut bytes = [0x10; 16];
            bytes[0] = if n < 1000 { 0x01 } else { 0x05 };
            bytes[1..5].copy_from_slice(&n.to_be_bytes());
            let id = SubmissionId::from_bytes(bytes);
            relational::put(
                &db,
                ACCOUNT,
                Key::Submission(id),
                Row::Submission(submission(2)),
                Sequence::from_u64(1),
            )
            .unwrap();
            relational::put(
                &db,
                ACCOUNT,
                Key::Recipient(id, 0),
                Row::Recipient(queued()),
                Sequence::from_u64(1),
            )
            .unwrap();
        }
        // The same group ID in another account has no valid recipient zero.
        db.execute(
            "INSERT INTO blob_ids VALUES(?1,?2)",
            params![other.as_bytes().as_slice(), BLOB.as_bytes().as_slice()],
        )
        .unwrap();
        db.execute("INSERT INTO blobs(account,id,kind,length,digest,created_at,changed) SELECT ?1,id,kind,length,digest,created_at,changed FROM blobs WHERE account=?2", params![other.as_bytes().as_slice(), ACCOUNT.as_bytes().as_slice()]).unwrap();
        relational::put(
            &db,
            other,
            Key::Submission(SUBMISSION),
            Row::Submission(submission(1)),
            Sequence::default(),
        )
        .unwrap();
        db.execute_batch("COMMIT").unwrap();
    }
    let key = recipient_key(0);
    let row = encode(Row::Recipient(queued()));
    let operation = Operation::put(Table::Recipients, &key, &row).unwrap();
    assert_eq!(
        store.commit(&td_crypto::Provider, request(1), &[operation], &mut []),
        Ok(Sequence::from_u64(2))
    );
    let writer = lock(&store.writer).unwrap();
    let consumed = VM_STEPS - lock(&writer.native.budget).unwrap().remaining;
    assert!(
        consumed < 10000,
        "unrelated groups consumed {consumed} VM steps"
    );
}

#[test]
fn expiry_inside_recipient_validation_rolls_back_the_whole_update() {
    use std::sync::atomic::{AtomicU64, Ordering};
    struct Timer(AtomicU64);
    impl Clock for Timer {
        fn sample(&self) -> Result<Time, ports::Error> {
            Ok(Time {
                utc_ms: 0,
                monotonic: Tick(self.0.load(Ordering::Relaxed)),
            })
        }
    }
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let clock = Arc::new(Timer(AtomicU64::new(1)));
    let store = IndexStore::create(
        &mut root,
        StoreEpoch::from_bytes([4; 16]),
        clock.clone(),
        1,
        deadline(),
    )
    .unwrap();
    store.create_account(ACCOUNT, deadline()).unwrap();
    create(&store, 2, &[0, 1]).unwrap();
    let key = recipient_key(0);
    let mut changed = queued();
    changed.diagnostic = "provisional";
    let row = encode(Row::Recipient(changed));
    let hook = clock.clone();
    let reads = Arc::new(AtomicU64::new(0));
    let hits = Arc::clone(&reads);
    lock(&lock(&store.writer).unwrap().native.connection)
        .unwrap()
        .authorizer(Some(move |context: AuthContext<'_>| {
            // FK checks read child keys; only group validation selects its state.
            if matches!(
                context.action,
                AuthAction::Read {
                    table_name: "recipients",
                    column_name: "state",
                    ..
                }
            ) {
                hits.fetch_add(1, Ordering::Relaxed);
                hook.0.store(100, Ordering::Relaxed);
            }
            Authorization::Allow
        }))
        .unwrap();
    let sub = encode(Row::Submission(submission(2)));
    assert_eq!(
        store.commit(
            &td_crypto::Provider,
            request(1),
            &[Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &sub).unwrap()],
            &mut []
        ),
        Err(CommitError::Rejected(ports::Error::Deadline))
    );
    assert_eq!(clock.0.load(Ordering::Relaxed), 100);
    assert_eq!(reads.load(Ordering::Relaxed), 1);
    lock(&lock(&store.writer).unwrap().native.connection)
        .unwrap()
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
    clock.0.store(1, Ordering::Relaxed);
    let view = store.view(ACCOUNT, deadline()).unwrap();
    assert_eq!(view.identity().committed_sequence, Sequence::from_u64(1));
    drop(view);
    assert_eq!(
        store.commit(
            &td_crypto::Provider,
            request(1),
            &[Operation::put(Table::Recipients, &key, &row).unwrap()],
            &mut []
        ),
        Ok(Sequence::from_u64(2))
    );
}

fn commit_encoded(
    store: &IndexStore<'_>,
    request: CommitRequest,
    operations: &[Operation<'_>],
    sources: &mut [BlobSource<'_>],
) -> Result<Sequence, CommitError> {
    let length: usize = operations.iter().map(|op| op.encoded_len().unwrap()).sum();
    let mut bytes = vec![0; length];
    let mut offset = 0;
    for operation in operations {
        offset += operation.encode(&mut bytes[offset..]).unwrap();
    }
    let mut slots = vec![None; operations.len()];
    let batch = crate::format::batch::Batch::decode(
        ports::TransactionInput {
            bytes: &bytes,
            count: operations.len(),
        },
        &mut slots,
    )
    .unwrap();
    store.commit_batch(&td_crypto::Provider, request, &batch, sources)
}

#[test]
fn encoded_batches_share_body_rollback_group_validation_and_snapshots() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store = open(&mut root);
    rejected(create_with(&store, 2, &[0], true));
    let ordinals: Vec<_> = (0..1000).collect();
    create_with(&store, 1000, &ordinals, true).unwrap();
    let mut old = store.view(ACCOUNT, deadline()).unwrap();
    let key = recipient_key(999);
    let mut changed = queued();
    changed.diagnostic = "encoded update";
    let row = encode(Row::Recipient(changed));
    let operation = Operation::put(Table::Recipients, &key, &row).unwrap();
    let repeated = vec![operation; 1000];
    assert_eq!(
        commit_encoded(&store, request(1), &repeated, &mut []),
        Ok(Sequence::from_u64(2))
    );
    assert_eq!(
        commit_encoded(&store, request(1), &[operation], &mut []),
        Err(CommitError::Rejected(ports::Error::Conflict))
    );
    rejected(commit_encoded(
        &store,
        request(2),
        &[Operation::delete(Table::Recipients, &key).unwrap()],
        &mut [],
    ));
    let mut bytes = [0; 1024];
    assert!(
        matches!(old.get(Key::Recipient(SUBMISSION, 999), &mut bytes).unwrap(), Some((Row::Recipient(row), _)) if row.diagnostic.is_empty())
    );
    let mut fresh = store.view(ACCOUNT, deadline()).unwrap();
    assert_eq!(fresh.identity().committed_sequence, Sequence::from_u64(2));
    assert!(
        matches!(fresh.get(Key::Recipient(SUBMISSION, 999), &mut bytes).unwrap(), Some((Row::Recipient(row), _)) if row.diagnostic == "encoded update")
    );
    drop(old);
    drop(fresh);
    store.validate_integrity(deadline()).unwrap();
}

fn blob_value(body: &[u8]) -> Vec<u8> {
    let mut digest = td_crypto::Provider.sha256().unwrap();
    digest.update(body).unwrap();
    encode(Row::Blob(BlobRow {
        kind: BlobKind::Message,
        length: body.len() as u64,
        digest: digest.finish().unwrap(),
        created_at: 0,
    }))
}
#[test]
fn encoded_source_matching_refuses_delete_duplicates_and_unmatched_sources() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store = open(&mut root);
    let body = b"encoded source";
    let row = blob_value(body);
    let put = Operation::put(Table::Blobs, BLOB.as_bytes(), &row).unwrap();
    let other = BlobId::from_bytes([99; 16]);
    for operations in [
        vec![Operation::delete(Table::Blobs, BLOB.as_bytes()).unwrap()],
        vec![put, put],
        vec![Operation::delete(Table::Blobs, other.as_bytes()).unwrap()],
    ] {
        let mut input = body.as_slice();
        assert_eq!(
            commit_encoded(
                &store,
                request(0),
                &operations,
                &mut [BlobSource {
                    id: BLOB,
                    source: &mut input
                }]
            ),
            Err(CommitError::Rejected(ports::Error::Invalid))
        );
        assert_eq!(input, body);
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        assert_eq!(view.identity().committed_sequence, Sequence::default());
        assert!(view.get(Key::Blob(BLOB), &mut [0; 128]).unwrap().is_none());
        drop(view);
        store.validate_integrity(deadline()).unwrap();
    }
}
#[test]
fn encoded_deadline_after_body_chunk_rolls_back_and_preserves_blob_id_reuse() {
    use std::{
        io::Read,
        sync::atomic::{AtomicU64, Ordering},
    };
    struct Timer(AtomicU64);
    impl Clock for Timer {
        fn sample(&self) -> Result<Time, ports::Error> {
            Ok(Time {
                utc_ms: 0,
                monotonic: Tick(self.0.load(Ordering::Relaxed)),
            })
        }
    }
    struct Expiring<'a> {
        bytes: &'a [u8],
        clock: Arc<Timer>,
    }
    impl Read for Expiring<'_> {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            let count = self.bytes.read(output)?;
            if count == 0 {
                self.clock.0.store(100, Ordering::Relaxed);
            }
            Ok(count)
        }
    }
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let clock = Arc::new(Timer(AtomicU64::new(1)));
    let store = IndexStore::create(
        &mut root,
        StoreEpoch::from_bytes([4; 16]),
        clock.clone(),
        1,
        deadline(),
    )
    .unwrap();
    store.create_account(ACCOUNT, deadline()).unwrap();
    let body = b"chunk inserted before EOF expiry";
    let row = blob_value(body);
    let operation = Operation::put(Table::Blobs, BLOB.as_bytes(), &row).unwrap();
    let mut source = Expiring {
        bytes: body,
        clock: clock.clone(),
    };
    assert_eq!(
        commit_encoded(
            &store,
            request(0),
            &[operation],
            &mut [BlobSource {
                id: BLOB,
                source: &mut source
            }]
        ),
        Err(CommitError::Rejected(ports::Error::Deadline))
    );
    assert!(source.bytes.is_empty());
    clock.0.store(1, Ordering::Relaxed);
    let mut view = store.view(ACCOUNT, deadline()).unwrap();
    assert_eq!(view.identity().committed_sequence, Sequence::default());
    assert!(view.get(Key::Blob(BLOB), &mut [0; 128]).unwrap().is_none());
    drop(view);
    let mut source = body.as_slice();
    assert_eq!(
        commit_encoded(
            &store,
            request(0),
            &[operation],
            &mut [BlobSource {
                id: BLOB,
                source: &mut source
            }]
        ),
        Ok(Sequence::from_u64(1))
    );
    store.validate_integrity(deadline()).unwrap();
}

#[path = "history_tests.rs"]
mod history_tests;

fn apply(
    store: &IndexStore<'_>,
    sequence: u64,
    operations: &[Operation<'_>],
    encoded: bool,
) -> Result<Sequence, CommitError> {
    if encoded {
        commit_encoded(store, request(sequence), operations, &mut [])
    } else {
        store.commit(&td_crypto::Provider, request(sequence), operations, &mut [])
    }
}
fn assert_fresh_group(
    sub: SubmissionRow<'_>,
    recipients: &[(u32, RecipientRow<'_>)],
    encoded: bool,
) {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store = open(&mut root);
    assert_eq!(
        create_group(&store, sub, recipients, encoded),
        Ok(Sequence::from_u64(1))
    );
}

#[path = "phase_tests.rs"]
mod phase_tests;
