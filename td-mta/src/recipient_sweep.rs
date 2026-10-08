//! Recipient coverage and queue consistency in one supplied final view.
#[path = "recipient_sweep/state.rs"]
mod state;
use crate::{
    format::{self, key::Key, row::Row, Table},
    ids::SubmissionId,
    ports::{self, ReadView, ViewIdentity},
};
pub(crate) use state::Group as QueueGroup;
pub use state::QueueError;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    View(ports::Error),
    Format(format::Error),
    ChangedView,
    Queue {
        submission: SubmissionId,
        ordinal: Option<u32>,
        error: QueueError,
    },
    Missing {
        submission: SubmissionId,
        ordinal: u32,
    },
    Unexpected {
        submission: SubmissionId,
        ordinal: u32,
    },
    RowLimit,
    Failed,
    Incomplete,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "recipient coverage: {self:?}")
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::View(e) => Some(e),
            Self::Format(e) => Some(e),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Step {
    Submission {
        id: SubmissionId,
        count: u32,
    },
    Recipient {
        submission: SubmissionId,
        ordinal: u32,
    },
    SubmissionsEnd,
    Complete,
}
#[derive(Clone, Copy)]
enum Phase {
    Submission,
    Recipient,
}
struct Group {
    id: SubmissionId,
    count: u32,
    next: u32,
    state: state::Group,
}
pub struct Sweep {
    identity: ViewIdentity,
    phase: Phase,
    group: Option<Group>,
    last_submission: Option<SubmissionId>,
    last_recipient: Option<(SubmissionId, u32)>,
    max_rows: u64,
    rows: u64,
    submissions: u64,
    recipients: u64,
    complete: bool,
    failed: bool,
}
impl Sweep {
    pub const fn new(identity: ViewIdentity, max_rows: u64) -> Self {
        Self {
            identity,
            phase: Phase::Submission,
            group: None,
            last_submission: None,
            last_recipient: None,
            max_rows,
            rows: 0,
            submissions: 0,
            recipients: 0,
            complete: false,
            failed: false,
        }
    }
    pub const fn is_failed(&self) -> bool {
        self.failed
    }
    pub const fn is_complete(&self) -> bool {
        self.complete && !self.failed
    }
    /// At most one next; the caller admits its full work and deadline.
    pub fn advance<V: ReadView + ?Sized>(
        &mut self,
        view: &mut V,
        key: &mut [u8],
        value: &mut [u8],
    ) -> Result<Step, Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        self.failed = true;
        let step = self.work(view, key, value)?;
        self.failed = false;
        Ok(step)
    }
    fn work<V: ReadView + ?Sized>(
        &mut self,
        view: &mut V,
        key: &mut [u8],
        value: &mut [u8],
    ) -> Result<Step, Error> {
        if view.identity() != self.identity {
            return Err(Error::ChangedView);
        }
        if self.complete {
            return Ok(Step::Complete);
        }
        let (table, prior) = match self.phase {
            Phase::Submission => (
                Table::Submissions,
                self.last_submission.map(Key::Submission),
            ),
            Phase::Recipient => (
                Table::Recipients,
                self.last_recipient.map(|(id, n)| Key::Recipient(id, n)),
            ),
        };
        let mut cursor = [0; 20];
        let after = if let Some(prior) = prior {
            let n = prior.encode(&mut cursor).map_err(Error::Format)?;
            Some(cursor.get(..n).ok_or(Error::Format(format::Error::Limit))?)
        } else {
            None
        };
        let found = view.next(table, after, key, value);
        if view.identity() != self.identity {
            return Err(Error::ChangedView);
        }
        let Some(record) = found.map_err(Error::View)? else {
            return match self.phase {
                Phase::Submission => {
                    self.group = None;
                    self.phase = Phase::Recipient;
                    Ok(Step::SubmissionsEnd)
                }
                Phase::Recipient => {
                    if let Some(group) = &self.group {
                        return Err(Error::Missing {
                            submission: group.id,
                            ordinal: group.next,
                        });
                    }
                    self.complete = true;
                    Ok(Step::Complete)
                }
            };
        };
        if record.key.table() != table || record.last_change > self.identity.committed_sequence {
            return Err(Error::Format(format::Error::InvalidValue));
        }
        record.row.validate_key(record.key).map_err(Error::Format)?;
        let step = match (self.phase, record.key, record.row) {
            (Phase::Submission, Key::Submission(id), Row::Submission(row)) => {
                if self.last_submission.is_some_and(|prior| prior >= id) {
                    return Err(Error::Format(format::Error::InvalidValue));
                }
                if self.rows >= self.max_rows {
                    return Err(Error::RowLimit);
                }
                self.last_submission = Some(id);
                self.group = Some(Group {
                    id,
                    count: row.recipient_count,
                    next: 0,
                    state: state::Group::new(row),
                });
                self.phase = Phase::Recipient;
                self.submissions = self
                    .submissions
                    .checked_add(1)
                    .ok_or(Error::Format(format::Error::Overflow))?;
                Step::Submission {
                    id,
                    count: row.recipient_count,
                }
            }
            (Phase::Recipient, Key::Recipient(id, ordinal), Row::Recipient(row)) => {
                if self
                    .last_recipient
                    .is_some_and(|prior| prior >= (id, ordinal))
                {
                    return Err(Error::Format(format::Error::InvalidValue));
                }
                let Some(group) = self.group.as_mut() else {
                    return Err(Error::Unexpected {
                        submission: id,
                        ordinal,
                    });
                };
                if id < group.id {
                    return Err(Error::Unexpected {
                        submission: id,
                        ordinal,
                    });
                }
                if id != group.id || ordinal != group.next {
                    return Err(Error::Missing {
                        submission: group.id,
                        ordinal: group.next,
                    });
                }
                group.state.recipient(row).map_err(|error| Error::Queue {
                    submission: id,
                    ordinal: Some(ordinal),
                    error,
                })?;
                if self.rows >= self.max_rows {
                    return Err(Error::RowLimit);
                }
                group.next = group
                    .next
                    .checked_add(1)
                    .ok_or(Error::Format(format::Error::Overflow))?;
                if group.next == group.count {
                    group.state.finish().map_err(|error| Error::Queue {
                        submission: id,
                        ordinal: None,
                        error,
                    })?;
                    self.group = None;
                    self.phase = Phase::Submission;
                }
                self.last_recipient = Some((id, ordinal));
                self.recipients = self
                    .recipients
                    .checked_add(1)
                    .ok_or(Error::Format(format::Error::Overflow))?;
                Step::Recipient {
                    submission: id,
                    ordinal,
                }
            }
            _ => return Err(Error::Format(format::Error::InvalidValue)),
        };
        self.rows = self
            .rows
            .checked_add(1)
            .ok_or(Error::Format(format::Error::Overflow))?;
        Ok(step)
    }
    pub fn finish(self) -> Result<CompleteCoverage, Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        if !self.complete {
            return Err(Error::Incomplete);
        }
        Ok(CompleteCoverage {
            identity: self.identity,
            submissions: self.submissions,
            recipients: self.recipients,
        })
    }
}
/// Coverage and current queue consistency; physical completeness and transition authority stay external.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompleteCoverage {
    identity: ViewIdentity,
    submissions: u64,
    recipients: u64,
}
impl CompleteCoverage {
    pub const fn identity(self) -> ViewIdentity {
        self.identity
    }
    pub const fn submissions(self) -> u64 {
        self.submissions
    }
    pub const fn recipients(self) -> u64 {
        self.recipients
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        format::{row::*, ObjectType, Sequence},
        ids::*,
        ports::{ChangeCursor, ChangeStep, Record},
    };
    fn id(n: u8) -> SubmissionId {
        SubmissionId::from_bytes([n; 16])
    }
    fn identity() -> ViewIdentity {
        ViewIdentity {
            account: AccountId::from_bytes([1; 16]),
            epoch: StoreEpoch::from_bytes([2; 16]),

            committed_sequence: Sequence::from_u64(2),
            history_floor: Sequence::default(),
        }
    }
    pub(super) fn submission(count: u32) -> Row<'static> {
        Row::Submission(SubmissionRow {
            email: EmailId::from_bytes([1; 16]),
            thread: ThreadId::from_bytes([2; 16]),
            identity: IdentityId::from_bytes([3; 16]),
            transmitted_blob: BlobId::from_bytes([4; 16]),
            reverse_path: "from@example.test",
            send_at: 0,
            expires_at: 200,
            recipient_count: count,
            completed_at: None,
            notification: NotificationState::None,
            notification_email: None,
        })
    }
    pub(super) fn recipient() -> Row<'static> {
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
        })
    }
    struct View<'a> {
        identity: ViewIdentity,
        submissions: &'a [(u8, u32)],
        recipients: &'a [(u8, u32)],
        calls: usize,
        moved: bool,
        absent: bool,
        error: bool,
        wrong_row: bool,
        wrong_table: bool,
        future: bool,
        repeat: Option<Table>,
        submission_state: Option<SubmissionRow<'static>>,
        recipient_state: Option<RecipientRow<'static>>,
    }
    impl<'a> View<'a> {
        fn new(submissions: &'a [(u8, u32)], recipients: &'a [(u8, u32)]) -> Self {
            Self {
                identity: identity(),
                submissions,
                recipients,
                calls: 0,
                moved: false,
                absent: false,
                error: false,
                wrong_row: false,
                wrong_table: false,
                future: false,
                repeat: None,
                submission_state: None,
                recipient_state: None,
            }
        }
    }
    impl ReadView for View<'_> {
        fn identity(&self) -> ViewIdentity {
            self.identity
        }
        fn next_change(
            &mut self,
            _: ChangeCursor,
            _: ObjectType,
        ) -> Result<ChangeStep, ports::Error> {
            Err(ports::Error::Invalid)
        }
        fn get<'a>(
            &mut self,
            _: Key<'_>,
            _: &'a mut [u8],
        ) -> Result<Option<(Row<'a>, Sequence)>, ports::Error> {
            Err(ports::Error::Invalid)
        }
        fn next<'a>(
            &mut self,
            table: Table,
            after: Option<&[u8]>,
            key: &'a mut [u8],
            value: &'a mut [u8],
        ) -> Result<Option<Record<'a>>, ports::Error> {
            self.calls += 1;
            if self.moved {
                self.identity.epoch = crate::ids::StoreEpoch::from_bytes([99; 16]);
            }
            if self.error {
                return Err(ports::Error::Capacity);
            }
            if self.absent {
                return Ok(None);
            }
            let rows = match table {
                Table::Submissions => self.submissions,
                Table::Recipients => self.recipients,
                _ => return Err(ports::Error::Invalid),
            };
            for &(identifier, n) in rows {
                let (source, row) = if table == Table::Submissions {
                    (
                        Key::Submission(id(identifier)),
                        self.submission_state
                            .map(Row::Submission)
                            .unwrap_or_else(|| submission(n)),
                    )
                } else {
                    (
                        Key::Recipient(id(identifier), n),
                        self.recipient_state
                            .map(Row::Recipient)
                            .unwrap_or_else(recipient),
                    )
                };
                let length = source.encode(key).map_err(|_| ports::Error::Capacity)?;
                if self.repeat != Some(table) && after.is_some_and(|prior| prior >= &key[..length])
                {
                    continue;
                }
                let source = if self.wrong_table {
                    Key::Thread(ThreadId::from_bytes([1; 16]))
                } else {
                    Key::decode(table, &key[..length]).unwrap()
                };
                let row = if self.wrong_row { Row::Thread } else { row };
                let length = row.encode(value).map_err(|_| ports::Error::Capacity)?;
                return Ok(Some(Record {
                    key: source,
                    row: Row::decode(row.table(), &value[..length]).unwrap(),
                    last_change: Sequence::from_u64(if self.future { 3 } else { 2 }),
                }));
            }
            Ok(None)
        }
    }
    pub(crate) fn probe() {
        for (submissions, recipients) in [
            (&[(1, 2), (2, 1)][..], &[(1, 0), (1, 1), (2, 0)][..]),
            (&[][..], &[][..]),
        ] {
            let mut view = View::new(submissions, recipients);
            let mut sweep = Sweep::new(identity(), (submissions.len() + recipients.len()) as u64);
            let mut complete = false;
            let mut sub_count = 0;
            let mut recipient_count = 0;
            for _ in 0..10 {
                let before = view.calls;
                match sweep
                    .advance(&mut view, &mut [0; 20], &mut [0; 512])
                    .unwrap()
                {
                    Step::Submission { .. } => sub_count += 1,
                    Step::Recipient { .. } => recipient_count += 1,
                    Step::SubmissionsEnd => {}
                    Step::Complete => {
                        complete = true;
                        break;
                    }
                }
                assert_eq!(view.calls, before + 1);
            }
            assert!(complete);
            assert!(sweep.is_complete());
            assert_eq!(
                (sub_count, recipient_count),
                (submissions.len(), recipients.len())
            );
            assert_eq!(view.calls, sub_count + recipient_count + 2);
            assert_eq!(
                sweep.advance(&mut view, &mut [], &mut []).unwrap(),
                Step::Complete
            );
            assert_eq!(view.calls, sub_count + recipient_count + 2);
            let complete = sweep.finish().unwrap();
            assert_eq!(complete.identity(), identity());
            assert_eq!(complete.submissions(), sub_count as u64);
            assert_eq!(complete.recipients(), recipient_count as u64);
        }
    }
    #[test]
    fn empty_view_and_multiple_and_maximum_groups_have_exact_coverage() {
        probe();
        let recipients: Vec<_> = (0..1000).map(|n| (1, n)).collect();
        let mut view = View::new(&[(1, 1000)], &recipients);
        let mut sweep = Sweep::new(identity(), 1001);
        for _ in 0..1003 {
            sweep
                .advance(&mut view, &mut [0; 20], &mut [0; 512])
                .unwrap();
        }
        assert!(sweep.is_complete());
        assert_eq!(view.calls, 1003);
        let complete = sweep.finish().unwrap();
        assert_eq!(complete.recipients(), 1000);
        assert!(std::mem::size_of::<Sweep>() <= 512);
    }
    #[test]
    fn missing_ordinals_groups_and_extra_orphan_rows_refuse() {
        type Case<'a> = (&'a [(u8, u32)], &'a [(u8, u32)], Error);
        let cases: [Case<'_>; 8] = [
            (
                &[(1, 1)],
                &[(1, 1)],
                Error::Missing {
                    submission: id(1),
                    ordinal: 0,
                },
            ),
            (
                &[(1, 2)],
                &[(1, 1)],
                Error::Missing {
                    submission: id(1),
                    ordinal: 0,
                },
            ),
            (
                &[(1, 3)],
                &[(1, 0), (1, 2)],
                Error::Missing {
                    submission: id(1),
                    ordinal: 1,
                },
            ),
            (
                &[(1, 2)],
                &[(1, 0)],
                Error::Missing {
                    submission: id(1),
                    ordinal: 1,
                },
            ),
            (
                &[(1, 1), (2, 1)],
                &[(2, 0)],
                Error::Missing {
                    submission: id(1),
                    ordinal: 0,
                },
            ),
            (
                &[(1, 1)],
                &[(1, 0), (1, 1)],
                Error::Unexpected {
                    submission: id(1),
                    ordinal: 1,
                },
            ),
            (
                &[(2, 1)],
                &[(1, 0), (2, 0)],
                Error::Unexpected {
                    submission: id(1),
                    ordinal: 0,
                },
            ),
            (
                &[],
                &[(1, 0)],
                Error::Unexpected {
                    submission: id(1),
                    ordinal: 0,
                },
            ),
        ];
        for (submissions, recipients, expected) in cases {
            let mut view = View::new(submissions, recipients);
            let mut sweep = Sweep::new(identity(), 10);
            let mut error = None;
            for _ in 0..12 {
                if let Err(e) = sweep.advance(&mut view, &mut [0; 20], &mut [0; 512]) {
                    error = Some(e);
                    break;
                }
            }
            assert_eq!(error, Some(expected));
            assert!(sweep.is_failed());
            let calls = view.calls;
            assert_eq!(
                sweep.advance(&mut view, &mut [], &mut []),
                Err(Error::Failed)
            );
            assert_eq!(view.calls, calls);
            assert!(matches!(sweep.finish(), Err(Error::Failed)));
        }
    }
    #[test]
    fn coverage_faults_precede_exhausted_row_allowance() {
        type Case<'a> = (&'a [(u8, u32)], &'a [(u8, u32)], u64, Error);
        let cases: [Case<'_>; 3] = [
            (
                &[(1, 2)],
                &[(1, 1)],
                1,
                Error::Missing {
                    submission: id(1),
                    ordinal: 0,
                },
            ),
            (
                &[(2, 1)],
                &[(1, 0)],
                1,
                Error::Unexpected {
                    submission: id(1),
                    ordinal: 0,
                },
            ),
            (
                &[(1, 1)],
                &[(1, 0), (1, 1)],
                2,
                Error::Unexpected {
                    submission: id(1),
                    ordinal: 1,
                },
            ),
        ];
        for (submissions, recipients, limit, expected) in cases {
            let mut view = View::new(submissions, recipients);
            let mut sweep = Sweep::new(identity(), limit);
            let mut error = None;
            for _ in 0..5 {
                if let Err(e) = sweep.advance(&mut view, &mut [0; 20], &mut [0; 512]) {
                    error = Some(e);
                    break;
                }
            }
            assert_eq!(error, Some(expected));
            assert!(sweep.is_failed());
            assert!(matches!(sweep.finish(), Err(Error::Failed)));
        }
    }
    #[test]
    fn identity_errors_and_bad_rows_retire_in_both_streams() {
        for in_recipient in [false, true] {
            for mode in 0..8 {
                let mut view = View::new(&[(1, 1)], &[(1, 0)]);
                let mut sweep = Sweep::new(identity(), u64::from(in_recipient));
                if in_recipient {
                    assert!(matches!(
                        sweep
                            .advance(&mut view, &mut [0; 20], &mut [0; 512])
                            .unwrap(),
                        Step::Submission { .. }
                    ));
                }
                let calls = view.calls;
                match mode {
                    0 => view.identity.epoch = crate::ids::StoreEpoch::from_bytes([99; 16]),
                    1 => view.moved = true,
                    2 => {
                        view.moved = true;
                        view.absent = true;
                    }
                    3 => {
                        view.moved = true;
                        view.error = true;
                    }
                    4 => view.error = true,
                    5 => view.wrong_row = true,
                    6 => view.wrong_table = true,
                    _ => view.future = true,
                }
                let error = sweep
                    .advance(&mut view, &mut [0; 20], &mut [0; 512])
                    .unwrap_err();
                match mode {
                    0..=3 => assert_eq!(error, Error::ChangedView),
                    4 => assert_eq!(error, Error::View(ports::Error::Capacity)),
                    _ => assert_eq!(error, Error::Format(format::Error::InvalidValue)),
                }
                assert_eq!(view.calls, calls + usize::from(mode != 0));
                assert!(sweep.is_failed());
                assert!(matches!(sweep.finish(), Err(Error::Failed)));
            }
        }
    }
    #[test]
    fn budgets_duplicate_keys_incomplete_and_postcomplete_view_changes_refuse() {
        for mode in 0..4 {
            let mut view = View::new(&[(1, 2)], &[(1, 0), (1, 1)]);
            let mut sweep = Sweep::new(identity(), if mode < 2 { mode } else { 3 });
            if mode == 2 {
                view.repeat = Some(Table::Submissions);
            }
            if mode == 3 {
                view.repeat = Some(Table::Recipients);
            }
            let mut error = None;
            for _ in 0..12 {
                if let Err(e) = sweep.advance(&mut view, &mut [0; 20], &mut [0; 512]) {
                    error = Some(e);
                    break;
                }
            }
            assert_eq!(
                error,
                Some(if mode < 2 {
                    Error::RowLimit
                } else {
                    Error::Format(format::Error::InvalidValue)
                })
            );
            assert!(matches!(sweep.finish(), Err(Error::Failed)));
        }
        assert!(matches!(
            Sweep::new(identity(), 0).finish(),
            Err(Error::Incomplete)
        ));
        let mut view = View::new(&[], &[]);
        let mut sweep = Sweep::new(identity(), 0);
        for _ in 0..2 {
            sweep
                .advance(&mut view, &mut [0; 20], &mut [0; 512])
                .unwrap();
        }
        view.identity.epoch = crate::ids::StoreEpoch::from_bytes([99; 16]);
        assert_eq!(
            sweep.advance(&mut view, &mut [], &mut []),
            Err(Error::ChangedView)
        );
        assert_eq!(view.calls, 2);
        assert!(matches!(sweep.finish(), Err(Error::Failed)));
    }
    #[test]
    fn queue_failures_retire_before_group_completion() {
        let Row::Submission(mut sub) = submission(1) else {
            panic!("fixture");
        };
        let Row::Recipient(mut rec) = recipient() else {
            panic!("fixture");
        };
        for mode in 0..3 {
            let mut view = View::new(&[(1, 1)], &[(1, 0)]);
            match mode {
                0 => {
                    rec.next_attempt_at = None;
                    view.recipient_state = Some(rec);
                }
                1 => {
                    sub.completed_at = Some(-1);
                    view.submission_state = Some(sub);
                }
                _ => {
                    sub.completed_at = None;
                    sub.notification = NotificationState::Pending;
                    view.submission_state = Some(sub);
                }
            }
            let mut sweep = Sweep::new(identity(), 2);
            assert!(matches!(
                sweep
                    .advance(&mut view, &mut [0; 20], &mut [0; 512])
                    .unwrap(),
                Step::Submission { .. }
            ));
            assert_eq!(
                sweep.advance(&mut view, &mut [0; 20], &mut [0; 512]),
                Err(Error::Queue {
                    submission: id(1),
                    ordinal: if mode == 0 { Some(0) } else { None },
                    error: match mode {
                        0 => QueueError::RecipientState,
                        1 => QueueError::Completion,
                        _ => QueueError::Notification,
                    },
                })
            );
            let calls = view.calls;
            assert_eq!(
                sweep.advance(&mut view, &mut [], &mut []),
                Err(Error::Failed)
            );
            assert_eq!(view.calls, calls);
            assert!(matches!(sweep.finish(), Err(Error::Failed)));
        }
    }
    #[test]
    fn completed_groups_and_canceled_notice_history_survive_full_sweep() {
        for state in [
            RecipientState::Failed,
            RecipientState::Canceled,
            RecipientState::OutcomeUnknown,
        ] {
            for notification in [NotificationState::Pending, NotificationState::Stored] {
                let Row::Submission(mut sub) = submission(1) else {
                    panic!("fixture");
                };
                sub.completed_at = Some(-1);
                sub.notification = notification;
                sub.notification_email = (notification == NotificationState::Stored)
                    .then_some(EmailId::from_bytes([9; 16]));
                let Row::Recipient(mut rec) = recipient() else {
                    panic!("fixture");
                };
                rec.state = state;
                rec.next_attempt_at = None;
                rec.reason = if state == RecipientState::Canceled {
                    FailureReason::Canceled
                } else {
                    FailureReason::Expired
                };
                if state == RecipientState::OutcomeUnknown {
                    rec.attempt = Some(AttemptId::from_bytes([8; 16]));
                    rec.attempt_count = 1;
                    rec.last_attempt_at = Some(0);
                    rec.phase = AttemptPhase::Final;
                    rec.uncertain = true;
                }
                let mut view = View::new(&[(1, 1)], &[(1, 0)]);
                view.submission_state = Some(sub);
                view.recipient_state = Some(rec);
                let mut sweep = Sweep::new(identity(), 2);
                for _ in 0..4 {
                    sweep
                        .advance(&mut view, &mut [0; 20], &mut [0; 512])
                        .unwrap();
                }
                assert!(sweep.is_complete());
                assert_eq!(sweep.finish().unwrap().recipients(), 1);
            }
        }
    }
}
