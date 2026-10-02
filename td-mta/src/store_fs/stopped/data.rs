//! Owned offline reference, queue, mailbox and blob validation.
use super::super::super::{BlobSweep, BlobSweepError, BlobSweepStep, CompleteBlobSweep};
use super::{CheckedFiles, ValidationReadRequest, ValidationView};
use crate::{
    format::{self, table::MAX_RECORD_BYTES, Table},
    frame_changes::Cell,
    mailbox_sweep,
    ports::{Clock, Crypto, Error as PolicyError, ViewIdentity},
    recipient_sweep, reference_sweep,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DataLimits {
    pub rows: u64,
    pub parent_reads: u64,
    pub blob_bytes: u64,
}
#[derive(Debug)]
pub enum DataError {
    Policy(PolicyError),
    References(reference_sweep::Error),
    Recipients(recipient_sweep::Error),
    Mailboxes(mailbox_sweep::Error),
    Blobs(BlobSweepError),
    Counts,
    Failed,
    Incomplete,
}
impl std::fmt::Display for DataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "stopped data validation: {self:?}")
    }
}
impl std::error::Error for DataError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Policy(e) => Some(e),
            Self::References(e) => Some(e),
            Self::Recipients(e) => Some(e),
            Self::Mailboxes(e) => Some(e),
            Self::Blobs(e) => Some(e),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataStep {
    References(reference_sweep::Step),
    Recipients(recipient_sweep::Step),
    Mailboxes(mailbox_sweep::Step),
    Blobs(BlobSweepStep),
    Complete,
}

pub struct DataValidation<'f, 'v, C: Crypto> {
    files: &'f CheckedFiles<'f, 'f, 'f, 'f, 'f>,
    view: ValidationView<'v, C>,
    references: Option<reference_sweep::Sweep>,
    recipients: Option<recipient_sweep::Sweep>,
    mailboxes: Option<mailbox_sweep::Sweep>,
    blobs: Option<BlobSweep<'v, 'v, C>>,
    checked_references: Option<reference_sweep::CompleteSweep>,
    checked_recipients: Option<recipient_sweep::CompleteCoverage>,
    checked_mailboxes: Option<mailbox_sweep::CompleteForest>,
    checked_blobs: Option<CompleteBlobSweep>,
    failed: bool,
    complete: bool,
}
impl CheckedFiles<'_, '_, '_, '_, '_> {
    /// The stopped owner outlives the scratch borrow, which ends on finish/drop.
    pub fn validate_data<'f: 'v, 'v, C: Crypto>(
        &'f self,
        crypto: &'v C,
        clock: &'v dyn Clock,
        request: ValidationReadRequest,
        record: &'v mut [u8; MAX_RECORD_BYTES],
        changes: &'v mut [Cell],
        limits: DataLimits,
    ) -> Result<DataValidation<'f, 'v, C>, DataError>
    where
        C::Sha256: Sync,
    {
        if self.tables.rows() > limits.rows {
            return Err(DataError::Policy(PolicyError::Capacity));
        }
        let view = self
            .read_view(crypto, clock, request, record, changes)
            .map_err(DataError::Policy)?;
        let identity = self.identity();
        Ok(DataValidation {
            files: self,
            references: Some(reference_sweep::Sweep::new(
                identity,
                view.utc_ms(),
                limits.rows,
            )),
            recipients: Some(recipient_sweep::Sweep::new(identity, limits.rows)),
            mailboxes: Some(mailbox_sweep::Sweep::new(
                identity,
                limits.rows,
                limits.parent_reads,
            )),
            blobs: Some(
                self.store
                    .blobs(crypto, identity, limits.rows, limits.blob_bytes),
            ),
            view,
            checked_references: None,
            checked_recipients: None,
            checked_mailboxes: None,
            checked_blobs: None,
            failed: false,
            complete: false,
        })
    }
}
impl<'f, C: Crypto> DataValidation<'f, '_, C>
where
    C::Sha256: Sync,
{
    pub const fn is_failed(&self) -> bool {
        self.failed
    }
    pub const fn is_complete(&self) -> bool {
        self.complete && !self.failed
    }
    /// One underlying sweep step, with deadline checks around its entire work.
    pub fn advance(&mut self, key: &mut [u8], value: &mut [u8]) -> Result<DataStep, DataError> {
        if self.failed {
            return Err(DataError::Failed);
        }
        self.failed = true;
        self.view.check_deadline().map_err(DataError::Policy)?;
        let result = self.work(key, value);
        // A failed ReadView already checked its final deadline and forbids more clock work.
        if !self.view.is_failed() {
            self.view.check_deadline().map_err(DataError::Policy)?;
        }
        let step = result?;
        self.failed = false;
        Ok(step)
    }
    fn work(&mut self, key: &mut [u8], value: &mut [u8]) -> Result<DataStep, DataError> {
        if self.complete {
            return Ok(DataStep::Complete);
        }
        if let Some(sweep) = self.references.as_mut() {
            let step = sweep
                .advance(&mut self.view, key, value)
                .map_err(DataError::References)?;
            if step == reference_sweep::Step::Complete {
                self.checked_references = Some(
                    self.references
                        .take()
                        .ok_or(DataError::Incomplete)?
                        .finish()
                        .map_err(DataError::References)?,
                );
            }
            return Ok(DataStep::References(step));
        }
        if let Some(sweep) = self.recipients.as_mut() {
            let step = sweep
                .advance(&mut self.view, key, value)
                .map_err(DataError::Recipients)?;
            if step == recipient_sweep::Step::Complete {
                self.checked_recipients = Some(
                    self.recipients
                        .take()
                        .ok_or(DataError::Incomplete)?
                        .finish()
                        .map_err(DataError::Recipients)?,
                );
            }
            return Ok(DataStep::Recipients(step));
        }
        if let Some(sweep) = self.mailboxes.as_mut() {
            let step = sweep
                .advance(&mut self.view, key, value)
                .map_err(DataError::Mailboxes)?;
            if step == mailbox_sweep::Step::Complete {
                self.checked_mailboxes = Some(
                    self.mailboxes
                        .take()
                        .ok_or(DataError::Incomplete)?
                        .finish()
                        .map_err(DataError::Mailboxes)?,
                );
            }
            return Ok(DataStep::Mailboxes(step));
        }
        if let Some(sweep) = self.blobs.as_mut() {
            let step = sweep
                .advance(&mut self.view, key, value)
                .map_err(DataError::Blobs)?;
            if step == BlobSweepStep::Complete {
                self.checked_blobs = Some(
                    self.blobs
                        .take()
                        .ok_or(DataError::Incomplete)?
                        .finish()
                        .map_err(DataError::Blobs)?,
                );
            }
            return Ok(DataStep::Blobs(step));
        }
        self.check_counts()?;
        self.complete = true;
        Ok(DataStep::Complete)
    }
    fn check_counts(&self) -> Result<(), DataError> {
        let refs = self.checked_references.ok_or(DataError::Incomplete)?;
        for tag in 1..=format::TABLE_COUNT {
            let table = Table::from_tag(u16::try_from(tag).map_err(|_| DataError::Counts)?)
                .map_err(|_| DataError::Counts)?;
            let index = tag.checked_sub(1).ok_or(DataError::Counts)?;
            if refs.table_rows(table) != self.files.tables.counts().get(index).copied() {
                return Err(DataError::Counts);
            }
        }
        let recipients = self.checked_recipients.ok_or(DataError::Incomplete)?;
        let mailboxes = self.checked_mailboxes.ok_or(DataError::Incomplete)?;
        let blobs = self.checked_blobs.ok_or(DataError::Incomplete)?;
        if refs.rows() != self.files.tables.rows()
            || Some(recipients.submissions()) != refs.table_rows(Table::Submissions)
            || Some(recipients.recipients()) != refs.table_rows(Table::Recipients)
            || Some(mailboxes.mailboxes()) != refs.table_rows(Table::Mailboxes)
            || Some(blobs.blobs()) != refs.table_rows(Table::Blobs)
        {
            return Err(DataError::Counts);
        }
        Ok(())
    }
    pub fn finish(mut self) -> Result<CheckedData<'f>, DataError> {
        if self.failed {
            return Err(DataError::Failed);
        }
        if !self.complete {
            return Err(DataError::Incomplete);
        }
        self.view.check_deadline().map_err(DataError::Policy)?;
        Ok(CheckedData {
            files: self.files,
            references: self.checked_references.ok_or(DataError::Incomplete)?,
            recipients: self.checked_recipients.ok_or(DataError::Incomplete)?,
            mailboxes: self.checked_mailboxes.ok_or(DataError::Incomplete)?,
            blobs: self.checked_blobs.ok_or(DataError::Incomplete)?,
        })
    }
}
/// Retains stopped ownership; recovery repair/accounting and activation are separate.
pub struct CheckedData<'f> {
    files: &'f CheckedFiles<'f, 'f, 'f, 'f, 'f>,
    references: reference_sweep::CompleteSweep,
    recipients: recipient_sweep::CompleteCoverage,
    mailboxes: mailbox_sweep::CompleteForest,
    blobs: CompleteBlobSweep,
}
impl CheckedData<'_> {
    pub fn identity(&self) -> ViewIdentity {
        self.files.identity()
    }
    pub const fn references(&self) -> reference_sweep::CompleteSweep {
        self.references
    }
    pub const fn recipients(&self) -> recipient_sweep::CompleteCoverage {
        self.recipients
    }
    pub const fn mailboxes(&self) -> mailbox_sweep::CompleteForest {
        self.mailboxes
    }
    pub const fn blobs(&self) -> CompleteBlobSweep {
        self.blobs
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::super::super::super::active::fixture;
    use super::super::tests::{limits as file_limits, prepare};
    use super::*;
    use crate::{
        format::{self, key::Key, operation::Operation, row::*, ObjectType, Sequence},
        ids::*,
        overlay,
        ports::{ChangeCursor, Deadline, Digest, Tick, Time},
        store_paths::{AccountEntry, Number},
    };
    use std::sync::atomic::{AtomicU64, Ordering};
    use td_crypto::Provider;
    struct TestClock {
        now: AtomicU64,
        calls: AtomicU64,
        expire: AtomicU64,
    }
    impl TestClock {
        fn new() -> Self {
            Self {
                now: AtomicU64::new(1),
                calls: AtomicU64::new(0),
                expire: AtomicU64::new(u64::MAX),
            }
        }
    }
    impl Clock for TestClock {
        fn sample(&self) -> Result<Time, PolicyError> {
            let call = self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(Time {
                utc_ms: 123,
                monotonic: Tick(if call >= self.expire.load(Ordering::Relaxed) {
                    100
                } else {
                    self.now.load(Ordering::Relaxed)
                }),
            })
        }
    }
    fn request() -> ValidationReadRequest {
        ValidationReadRequest {
            after: ChangeCursor {
                sequence: Sequence::default(),
                operation: u32::MAX,
            },
            kind: ObjectType::Email,
            deadline: Deadline::after(Tick(0), 100).unwrap(),
            limits: super::super::ReadLimits {
                table_bytes: 225,
                change_source_bytes: 4096,
                steps: 128,
            },
        }
    }
    fn limits() -> DataLimits {
        DataLimits {
            rows: 16,
            parent_reads: 16,
            blob_bytes: 3,
        }
    }
    fn put(payload: &mut Vec<u8>, key: Key<'_>, row: Row<'_>) {
        let mut key_bytes = [0; 1024];
        let key_len = key.encode(&mut key_bytes).unwrap();
        let mut value = [0; 1024];
        let len = row.encode(&mut value).unwrap();
        let op = Operation::put(key.table(), &key_bytes[..key_len], &value[..len]).unwrap();
        let mut bytes = vec![0; op.encoded_len().unwrap()];
        op.encode(&mut bytes).unwrap();
        payload.extend_from_slice(&bytes);
    }
    // Base frame deletes the literal checkpoint blob. Add an independently sealed final graph.
    fn graph(store: &super::super::super::StoppedStore, mode: u8) -> usize {
        let blob = BlobId::from_bytes([0x44; 16]);
        let thread = ThreadId::from_bytes([0x55; 16]);
        let email = EmailId::from_bytes([0x66; 16]);
        let mailbox = MailboxId::from_bytes([0x77; 16]);
        let submission = SubmissionId::from_bytes([0x88; 16]);
        let account = AccountId::from_bytes([0x33; 16]);
        let mut digest = Provider.sha256().unwrap();
        digest.update(b"abc").unwrap();
        let mut payload = Vec::new();
        put(
            &mut payload,
            Key::Blob(blob),
            Row::Blob(BlobRow {
                kind: BlobKind::Message,
                length: 3,
                digest: digest.finish().unwrap(),
                created_at: 0,
            }),
        );
        put(&mut payload, Key::Thread(thread), Row::Thread);
        put(
            &mut payload,
            Key::Email(email),
            Row::Email(EmailRow {
                blob,
                thread: if mode == 1 {
                    ThreadId::from_bytes([0x56; 16])
                } else {
                    thread
                },
                received_at: 0,
                origin: EmailOrigin::Jmap,
            }),
        );
        let parent = (mode == 2).then_some(MailboxId::from_bytes([0x78; 16]));
        put(
            &mut payload,
            Key::Mailbox(mailbox),
            Row::Mailbox(MailboxRow {
                name: "Inbox",
                parent,
                role: Some("inbox"),
                sort_order: 0,
                subscribed: true,
            }),
        );
        put(
            &mut payload,
            Key::Membership(email, mailbox),
            Row::Membership,
        );
        put(
            &mut payload,
            Key::Submission(submission),
            Row::Submission(SubmissionRow {
                email,
                thread,
                identity: IdentityId::from_bytes([0x99; 16]),
                transmitted_blob: blob,
                reverse_path: "from@example.test",
                send_at: 0,
                expires_at: 432000000,
                recipient_count: 1,
                completed_at: (mode == 3).then_some(0),
                notification: NotificationState::None,
                notification_email: None,
            }),
        );
        put(
            &mut payload,
            Key::Recipient(submission, 0),
            Row::Recipient(RecipientRow {
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
        );
        if let Some(parent) = parent {
            put(
                &mut payload,
                Key::Mailbox(parent),
                Row::Mailbox(MailboxRow {
                    name: "cycle",
                    parent: Some(mailbox),
                    role: None,
                    sort_order: 0,
                    subscribed: true,
                }),
            );
        }
        let mut frame = vec![0; format::FRAME_HEADER_BYTES];
        frame.extend_from_slice(&payload);
        frame.resize(frame.len() + format::FRAME_FOOTER_BYTES, 0);
        format::frame::seal(
            &Provider,
            Sequence::from_u64(3),
            if mode == 2 { 8 } else { 7 },
            &mut frame,
        )
        .unwrap();
        let mut journal = fixture::journal();
        journal.extend_from_slice(&frame);
        fixture::write(&store.root, &journal);
        if mode != 4 {
            for entry in [AccountEntry::Messages, AccountEntry::Temporary] {
                store.root.create_account_directory(account, entry).unwrap();
            }
            store
                .root
                .create_account_directory(account, AccountEntry::Shard(BlobKind::Message, 0x44))
                .unwrap();
            let mut file = store
                .root
                .create_temporary(account, Number::new(900).unwrap(), 3)
                .unwrap();
            file.write(if mode == 5 { b"abd" } else { b"abc" }).unwrap();
            file.sync()
                .unwrap()
                .publish_blob(BlobKind::Message, blob)
                .unwrap();
        }
        journal.len()
    }
    #[test]
    fn populated_and_empty_selected_graphs_complete_and_release_scratch() {
        for empty in [false, true] {
            let (_fixture, store, bytes) = prepare();
            let end = if empty { 256 } else { graph(&store, 0) };
            let mut frames = [0; 4096];
            let mut overlay = [overlay::Cell::EMPTY; 16];
            let active = store
                .load_active_overlay(
                    &Provider,
                    bytes.selection(),
                    bytes.view(if empty { 2 } else { 3 }, end as u64),
                    4096,
                    &mut frames,
                    &mut overlay,
                )
                .unwrap();
            let mut record = [0; MAX_RECORD_BYTES];
            let mut changes = [Cell::EMPTY; 2];
            let mut fl = file_limits();
            fl.tables.rows = 16;
            let mut physical = store
                .validate_files(
                    &Provider,
                    bytes.selection(),
                    &active,
                    &mut record,
                    &mut changes,
                    fl,
                )
                .unwrap();
            for _ in 0..100 {
                physical.advance().unwrap();
                if physical.is_complete() {
                    break;
                }
            }
            let (files, record, changes) = physical.finish().unwrap();
            let clock = TestClock::new();
            let mut check = files
                .validate_data(&Provider, &clock, request(), record, changes, limits())
                .unwrap();
            assert!(std::mem::size_of_val(&check) <= 8192);
            let mut key = [0; 1024];
            let mut value = [0; 1024];
            let mut phase = 0;
            for _ in 0..100 {
                let step = check.advance(&mut key, &mut value).unwrap();
                let current = match step {
                    DataStep::References(_) => 0,
                    DataStep::Recipients(_) => 1,
                    DataStep::Mailboxes(_) => 2,
                    DataStep::Blobs(_) => 3,
                    DataStep::Complete => 4,
                };
                assert!(current >= phase);
                phase = current;
                if check.is_complete() {
                    break;
                }
            }
            assert!(check.is_complete());
            assert_eq!(phase, 4);
            assert_eq!(check.advance(&mut [], &mut []).unwrap(), DataStep::Complete);
            let complete = check.finish().unwrap();
            // Proof borrows only the stopped files, so both original scratch regions are reusable.
            record.fill(0);
            changes.fill(Cell::EMPTY);
            assert_eq!(complete.identity(), active.identity());
            assert_eq!(complete.references().rows(), if empty { 0 } else { 7 });
            assert_eq!(complete.references().utc_ms(), 123);
            assert_eq!(complete.recipients().submissions(), u64::from(!empty));
            assert_eq!(complete.recipients().recipients(), u64::from(!empty));
            assert_eq!(complete.mailboxes().mailboxes(), u64::from(!empty));
            assert_eq!(complete.blobs().blobs(), u64::from(!empty));
            assert_eq!(complete.blobs().bytes(), if empty { 0 } else { 3 });
        }
    }
    #[test]
    fn references_queue_cycles_blob_failures_and_budgets_retire_validation() {
        for mode in 1..=10 {
            let (_fixture, store, bytes) = prepare();
            let end = graph(&store, if mode < 6 { mode } else { 0 });
            let mut frames = [0; 4096];
            let mut overlay = [overlay::Cell::EMPTY; 16];
            let active = store
                .load_active_overlay(
                    &Provider,
                    bytes.selection(),
                    bytes.view(3, end as u64),
                    4096,
                    &mut frames,
                    &mut overlay,
                )
                .unwrap();
            let mut record = [0; MAX_RECORD_BYTES];
            let mut changes = [Cell::EMPTY; 2];
            let mut fl = file_limits();
            fl.tables.rows = 16;
            let mut physical = store
                .validate_files(
                    &Provider,
                    bytes.selection(),
                    &active,
                    &mut record,
                    &mut changes,
                    fl,
                )
                .unwrap();
            for _ in 0..100 {
                physical.advance().unwrap();
                if physical.is_complete() {
                    break;
                }
            }
            let (files, record, changes) = physical.finish().unwrap();
            let clock = TestClock::new();
            let mut budget = limits();
            if mode == 6 {
                budget.rows = 6;
            }
            if mode == 7 {
                budget.parent_reads = 0;
            }
            if mode == 8 {
                budget.blob_bytes = 2;
            }
            let mut req = request();
            if mode == 10 {
                req.limits.steps = 1;
            }
            let result = files.validate_data(&Provider, &clock, req, record, changes, budget);
            if mode == 6 {
                assert!(matches!(
                    result,
                    Err(DataError::Policy(PolicyError::Capacity))
                ));
                continue;
            }
            let mut check = result.unwrap();
            if mode == 9 {
                clock.now.store(100, Ordering::Relaxed);
            }
            let before = clock.calls.load(Ordering::Relaxed);
            let mut error = None;
            for _ in 0..100 {
                if let Err(e) = check.advance(&mut [0; 1024], &mut [0; 1024]) {
                    error = Some(e);
                    break;
                }
            }
            match mode {
                1 => assert!(matches!(error, Some(DataError::References(_)))),
                2 => assert!(matches!(
                    error,
                    Some(DataError::Mailboxes(mailbox_sweep::Error::Parent(
                        crate::mailbox_parents::Error::Cycle
                    )))
                )),
                7 => assert!(matches!(
                    error,
                    Some(DataError::Mailboxes(mailbox_sweep::Error::Parent(
                        crate::mailbox_parents::Error::ReadLimit
                    )))
                )),
                3 => assert!(
                    matches!(error, Some(DataError::Recipients(recipient_sweep::Error::Queue { submission, ordinal: None, error: recipient_sweep::QueueError::Completion })) if submission == SubmissionId::from_bytes([0x88; 16]))
                ),
                4 | 5 => assert!(matches!(
                    error,
                    Some(DataError::Blobs(BlobSweepError::Input(_)))
                )),
                8 => assert!(matches!(
                    error,
                    Some(DataError::Blobs(BlobSweepError::ByteLimit))
                )),
                10 => {
                    assert!(matches!(
                        error,
                        Some(DataError::References(reference_sweep::Error::View(
                            PolicyError::Capacity
                        )))
                    ));
                    assert_eq!(clock.calls.load(Ordering::Relaxed), before + 6);
                }
                9 => assert!(matches!(
                    error,
                    Some(DataError::Policy(PolicyError::Deadline))
                )),
                _ => panic!("case"),
            }
            assert!(check.is_failed());
            assert!(!check.is_complete());
            let calls = clock.calls.load(Ordering::Relaxed);
            assert!(matches!(
                check.advance(&mut [], &mut []),
                Err(DataError::Failed)
            ));
            assert_eq!(clock.calls.load(Ordering::Relaxed), calls);
            assert!(matches!(check.finish(), Err(DataError::Failed)));
        }
    }
    #[test]
    fn blob_steps_and_consuming_completion_check_deadlines() {
        for mode in 0..4 {
            let (_fixture, store, bytes) = prepare();
            let end = graph(&store, 0);
            let mut frames = [0; 4096];
            let mut overlay = [overlay::Cell::EMPTY; 16];
            let active = store
                .load_active_overlay(
                    &Provider,
                    bytes.selection(),
                    bytes.view(3, end as u64),
                    4096,
                    &mut frames,
                    &mut overlay,
                )
                .unwrap();
            let mut record = [0; MAX_RECORD_BYTES];
            let mut changes = [Cell::EMPTY; 2];
            let mut fl = file_limits();
            fl.tables.rows = 16;
            let mut physical = store
                .validate_files(
                    &Provider,
                    bytes.selection(),
                    &active,
                    &mut record,
                    &mut changes,
                    fl,
                )
                .unwrap();
            for _ in 0..100 {
                physical.advance().unwrap();
                if physical.is_complete() {
                    break;
                }
            }
            let (files, record, changes) = physical.finish().unwrap();
            let clock = TestClock::new();
            let mut check = files
                .validate_data(&Provider, &clock, request(), record, changes, limits())
                .unwrap();
            if mode == 0 {
                assert!(matches!(check.finish(), Err(DataError::Incomplete)));
                continue;
            }
            let mut value = [0; 1024];
            for _ in 0..100 {
                let step = check.advance(&mut [0; 1024], &mut value).unwrap();
                if mode == 1 && step == DataStep::Blobs(BlobSweepStep::Opened) {
                    value.fill(0);
                    clock
                        .expire
                        .store(clock.calls.load(Ordering::Relaxed) + 2, Ordering::Relaxed);
                    assert!(matches!(
                        check.advance(&mut [0; 1024], &mut value),
                        Err(DataError::Policy(PolicyError::Deadline))
                    ));
                    assert_eq!(&value[..3], b"abc");
                    assert!(check.is_failed());
                    break;
                }
                if check.is_complete() {
                    break;
                }
            }
            if mode >= 2 {
                assert!(check.is_complete());
                clock.now.store(100, Ordering::Relaxed);
                if mode == 2 {
                    assert!(matches!(
                        check.finish(),
                        Err(DataError::Policy(PolicyError::Deadline))
                    ));
                } else {
                    assert!(matches!(
                        check.advance(&mut [], &mut []),
                        Err(DataError::Policy(PolicyError::Deadline))
                    ));
                    assert!(check.is_failed());
                    assert!(matches!(check.finish(), Err(DataError::Failed)));
                }
            } else {
                assert!(check.is_failed());
            }
        }
    }
}
