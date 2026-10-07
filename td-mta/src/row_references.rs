//! Bounded direct owning-reference checks over one caller-owned final view.
use crate::{
    format::{
        self,
        key::Key,
        row::{BlobKind, Row},
        Sequence,
    },
    ids::{BlobId, EmailId, MailboxId, SubmissionId, ThreadId},
    ports::{self, ReadView, ViewIdentity},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Target {
    Blob { id: BlobId, kind: BlobKind },
    Mailbox(MailboxId),
    Email(EmailId),
    Thread(ThreadId),
    Submission { id: SubmissionId, ordinal: u32 },
}
impl Target {
    pub(crate) fn key(self) -> Key<'static> {
        match self {
            Self::Blob { id, .. } => Key::Blob(id),
            Self::Mailbox(id) => Key::Mailbox(id),
            Self::Email(id) => Key::Email(id),
            Self::Thread(id) => Key::Thread(id),
            Self::Submission { id, .. } => Key::Submission(id),
        }
    }
    fn accepts(self, row: Row<'_>) -> bool {
        match (self, row) {
            (Self::Blob { kind, .. }, Row::Blob(blob)) => kind == blob.kind,
            (Self::Submission { ordinal, .. }, Row::Submission(submission)) => {
                ordinal < submission.recipient_count
            }
            (Self::Mailbox(_), Row::Mailbox(_))
            | (Self::Email(_), Row::Email(_))
            | (Self::Thread(_), Row::Thread) => true,
            _ => false,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    View(ports::Error),
    Format(format::Error),
    ChangedView,
    Missing(Target),
    InvalidTarget(Target),
    Failed,
    Incomplete,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "row reference validation: {self:?}")
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::View(error) => Some(error),
            Self::Format(error) => Some(error),
            _ => None,
        }
    }
}
pub struct ReferenceCheck<'k> {
    identity: ViewIdentity,
    source: Key<'k>,
    sequence: Sequence,
    utc_ms: i64,
    targets: [Option<Target>; 2],
    next: usize,
    complete: bool,
    failed: bool,
}
impl<'k> ReferenceCheck<'k> {
    pub(crate) fn targets(&self) -> &[Option<Target>; 2] {
        &self.targets
    }
    /// The caller supplies the final source row and an admitted wall-time sample.
    pub fn new(
        identity: ViewIdentity,
        source: Key<'k>,
        row: Row<'_>,
        sequence: Sequence,
        utc_ms: i64,
    ) -> Result<Self, Error> {
        row.validate_key(source).map_err(Error::Format)?;
        if sequence > identity.committed_sequence {
            return Err(Error::Format(format::Error::InvalidValue));
        }
        let targets = match (source, row) {
            (_, Row::Mailbox(row)) => [row.parent.map(Target::Mailbox), None],
            (_, Row::Email(row)) => [
                Some(Target::Blob {
                    id: row.blob,
                    kind: BlobKind::Message,
                }),
                Some(Target::Thread(row.thread)),
            ],
            (Key::Membership(email, mailbox), Row::Membership) => {
                [Some(Target::Email(email)), Some(Target::Mailbox(mailbox))]
            }
            (Key::Keyword(email, _), Row::Keyword)
            | (Key::ThreadAnchor(_, email), Row::ThreadAnchor) => {
                [Some(Target::Email(email)), None]
            }
            (_, Row::Submission(row)) => [
                Some(Target::Blob {
                    id: row.transmitted_blob,
                    kind: BlobKind::Message,
                }),
                None,
            ],
            (Key::Recipient(id, ordinal), Row::Recipient(_)) => {
                [Some(Target::Submission { id, ordinal }), None]
            }
            (Key::Lease(id), Row::Lease(row)) => {
                if row.account != identity.account {
                    return Err(Error::Format(format::Error::InvalidValue));
                }
                [
                    if row.expires_at > utc_ms {
                        Some(Target::Blob {
                            id,
                            kind: BlobKind::Upload,
                        })
                    } else {
                        None
                    },
                    None,
                ]
            }
            (_, Row::Blob(_) | Row::Thread | Row::Import(_)) => [None, None],
            _ => return Err(Error::Format(format::Error::InvalidValue)),
        };
        Ok(Self {
            identity,
            source,
            sequence,
            utc_ms,
            targets,
            next: 0,
            complete: false,
            failed: false,
        })
    }
    pub const fn is_failed(&self) -> bool {
        self.failed
    }
    pub const fn is_complete(&self) -> bool {
        self.complete && !self.failed
    }
    /// At most one get; identity movement takes precedence over lookup results/errors.
    pub fn advance<V: ReadView + ?Sized>(
        &mut self,
        view: &mut V,
        value: &mut [u8],
    ) -> Result<bool, Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        self.failed = true;
        if view.identity() != self.identity {
            return Err(Error::ChangedView);
        }
        if self.complete {
            self.failed = false;
            return Ok(true);
        }
        if let Some(target) = self.targets.get(self.next).copied().flatten() {
            let key = target.key();
            let found = view.get(key, value);
            if view.identity() != self.identity {
                return Err(Error::ChangedView);
            }
            let (row, sequence) = found.map_err(Error::View)?.ok_or(Error::Missing(target))?;
            if sequence > self.identity.committed_sequence {
                return Err(Error::Format(format::Error::InvalidValue));
            }
            row.validate_key(key).map_err(Error::Format)?;
            if !target.accepts(row) {
                return Err(Error::InvalidTarget(target));
            }
            self.next = self
                .next
                .checked_add(1)
                .ok_or(Error::Format(format::Error::Overflow))?;
        }
        self.complete = self.targets.get(self.next).copied().flatten().is_none();
        self.failed = false;
        Ok(self.complete)
    }
    pub fn finish(self) -> Result<CompleteReferences<'k>, Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        if !self.complete {
            return Err(Error::Incomplete);
        }
        Ok(CompleteReferences {
            identity: self.identity,
            source: self.source,
            sequence: self.sequence,
            utc_ms: self.utc_ms,
            reads: self.next,
        })
    }
}
/// Direct checks for supplied source data; this carries no file, graph or pin authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompleteReferences<'k> {
    identity: ViewIdentity,
    source: Key<'k>,
    sequence: Sequence,
    utc_ms: i64,
    reads: usize,
}
impl<'k> CompleteReferences<'k> {
    pub const fn identity(self) -> ViewIdentity {
        self.identity
    }
    pub const fn source(self) -> Key<'k> {
        self.source
    }
    pub const fn sequence(self) -> Sequence {
        self.sequence
    }
    pub const fn utc_ms(self) -> i64 {
        self.utc_ms
    }
    pub const fn reads(self) -> usize {
        self.reads
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        format::{key::SourceKind, row::*, ObjectType, Table},
        ids::{AccountId, DeviceId, IdentityId, InstanceId, StoreEpoch},
        ports::{ChangeCursor, ChangeStep, Record},
    };
    const BLOB: BlobId = BlobId::from_bytes([1; 16]);
    const UPLOAD: BlobId = BlobId::from_bytes([2; 16]);
    const MAILBOX: MailboxId = MailboxId::from_bytes([3; 16]);
    const THREAD: ThreadId = ThreadId::from_bytes([4; 16]);
    const EMAIL: EmailId = EmailId::from_bytes([5; 16]);
    const SUBMISSION: SubmissionId = SubmissionId::from_bytes([6; 16]);
    fn identity() -> ViewIdentity {
        ViewIdentity {
            account: AccountId::from_bytes([7; 16]),
            epoch: StoreEpoch::from_bytes([8; 16]),

            committed_sequence: Sequence::from_u64(2),
            history_floor: Sequence::default(),
        }
    }
    fn blob(kind: BlobKind) -> Row<'static> {
        Row::Blob(BlobRow {
            kind,
            length: 1,
            digest: [0; 32],
            created_at: 0,
        })
    }
    fn mailbox(parent: Option<MailboxId>) -> Row<'static> {
        Row::Mailbox(MailboxRow {
            name: "mail",
            parent,
            role: None,
            sort_order: 0,
            subscribed: true,
        })
    }
    fn email() -> Row<'static> {
        Row::Email(EmailRow {
            blob: BLOB,
            thread: THREAD,
            received_at: 0,
            origin: EmailOrigin::Jmap,
        })
    }
    fn submission() -> Row<'static> {
        Row::Submission(SubmissionRow {
            email: EmailId::from_bytes([99; 16]),
            thread: ThreadId::from_bytes([98; 16]),
            identity: IdentityId::from_bytes([97; 16]),
            transmitted_blob: BLOB,
            reverse_path: "sender@example.test",
            send_at: 0,
            expires_at: 200,
            recipient_count: 2,
            completed_at: Some(50),
            notification: NotificationState::Stored,
            notification_email: Some(EmailId::from_bytes([96; 16])),
        })
    }
    fn recipient() -> Row<'static> {
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
    fn lease(expires_at: i64) -> Row<'static> {
        Row::Lease(LeaseRow {
            account: identity().account,
            device: DeviceId::from_bytes([9; 16]),
            expires_at,
            uses: LeaseUse::Both,
        })
    }
    struct View {
        identity: ViewIdentity,
        rows: [(Key<'static>, Row<'static>); 6],
        calls: usize,
        missing: bool,
        error: Option<ports::Error>,
        moved: bool,
        sequence: Sequence,
    }
    impl View {
        fn new() -> Self {
            Self {
                identity: identity(),
                rows: [
                    (Key::Blob(BLOB), blob(BlobKind::Message)),
                    (Key::Blob(UPLOAD), blob(BlobKind::Upload)),
                    (Key::Mailbox(MAILBOX), mailbox(None)),
                    (Key::Thread(THREAD), Row::Thread),
                    (Key::Email(EMAIL), email()),
                    (Key::Submission(SUBMISSION), submission()),
                ],
                calls: 0,
                missing: false,
                error: None,
                moved: false,
                sequence: Sequence::from_u64(2),
            }
        }
    }
    impl ReadView for View {
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
        fn next<'a>(
            &mut self,
            _: Table,
            _: Option<&[u8]>,
            _: &'a mut [u8],
            _: &'a mut [u8],
        ) -> Result<Option<Record<'a>>, ports::Error> {
            Err(ports::Error::Invalid)
        }
        fn get<'a>(
            &mut self,
            key: Key<'_>,
            value: &'a mut [u8],
        ) -> Result<Option<(Row<'a>, Sequence)>, ports::Error> {
            self.calls += 1;
            if self.moved {
                self.identity.epoch = crate::ids::StoreEpoch::from_bytes([99; 16]);
            }
            if let Some(error) = self.error {
                return Err(error);
            }
            if self.missing {
                return Ok(None);
            }
            let Some((_, row)) = self.rows.iter().find(|(candidate, _)| *candidate == key) else {
                return Ok(None);
            };
            let n = row.encode(value).map_err(|_| ports::Error::Capacity)?;
            Ok(Some((
                Row::decode(row.table(), value.get(..n).unwrap()).unwrap(),
                self.sequence,
            )))
        }
    }
    pub(crate) fn probe() {
        let mut value = [0; 512];
        for (key, row, reads) in [
            (Key::Blob(BLOB), blob(BlobKind::Message), 0),
            (
                Key::Mailbox(MailboxId::from_bytes([10; 16])),
                mailbox(Some(MAILBOX)),
                1,
            ),
            (Key::Email(EMAIL), email(), 2),
            (Key::Membership(EMAIL, MAILBOX), Row::Membership, 2),
            (Key::Keyword(EMAIL, "$seen"), Row::Keyword, 1),
            (Key::Thread(THREAD), Row::Thread, 0),
            (
                Key::ThreadAnchor("message@example.test", EMAIL),
                Row::ThreadAnchor,
                1,
            ),
            (Key::Submission(SUBMISSION), submission(), 1),
            (Key::Recipient(SUBMISSION, 1), recipient(), 1),
            (Key::Lease(UPLOAD), lease(101), 1),
            (Key::Lease(UPLOAD), lease(100), 0),
            (
                Key::Import {
                    instance: InstanceId::from_bytes([11; 16]),
                    kind: SourceKind::Email,
                    account: b"a",
                    object: b"o",
                },
                Row::Import(ImportRow {
                    local_object: [99; 16],
                    historical_blob: Some(BlobId::from_bytes([98; 16])),
                    source_digest: [0; 32],
                }),
                0,
            ),
        ] {
            let mut view = View::new();
            let mut check =
                ReferenceCheck::new(identity(), key, row, Sequence::from_u64(2), 100).unwrap();
            for step in 0..reads.max(1) {
                assert_eq!(
                    check.advance(&mut view, &mut value).unwrap(),
                    step + 1 == reads.max(1)
                );
                assert_eq!(view.calls, (step + 1).min(reads));
            }
            assert!(check.is_complete());
            assert!(check.advance(&mut view, &mut value).unwrap());
            assert_eq!(view.calls, reads);
            let complete = check.finish().unwrap();
            assert_eq!(complete.source(), key);
            assert_eq!(complete.identity(), identity());
            assert_eq!(complete.sequence(), Sequence::from_u64(2));
            assert_eq!(complete.utc_ms(), 100);
            assert_eq!(complete.reads(), reads);
        }
        let mut view = View::new();
        view.missing = true;
        let mut check = ReferenceCheck::new(
            identity(),
            Key::Email(EMAIL),
            email(),
            Sequence::from_u64(2),
            100,
        )
        .unwrap();
        assert!(matches!(
            check.advance(&mut view, &mut value),
            Err(Error::Missing(_))
        ));
        assert!(check.is_failed());
        assert!(matches!(
            check.advance(&mut view, &mut value),
            Err(Error::Failed)
        ));
    }
    #[test]
    fn every_table_obeys_owning_and_historical_reference_rules() {
        probe();
        assert!(std::mem::size_of::<ReferenceCheck<'_>>() <= 512);
    }
    #[test]
    fn kind_ordinal_and_lease_account_refuse() {
        for (key, row) in [
            (Key::Email(EMAIL), email()),
            (Key::Lease(UPLOAD), lease(101)),
            (Key::Recipient(SUBMISSION, 2), recipient()),
        ] {
            let mut view = View::new();
            if key == Key::Email(EMAIL) {
                view.rows[0].1 = blob(BlobKind::Upload);
            }
            if key == Key::Lease(UPLOAD) {
                view.rows[1].1 = blob(BlobKind::Message);
            }
            let mut check =
                ReferenceCheck::new(identity(), key, row, Sequence::from_u64(1), 100).unwrap();
            assert!(matches!(
                check.advance(&mut view, &mut [0; 512]),
                Err(Error::InvalidTarget(_))
            ));
            assert!(matches!(check.finish(), Err(Error::Failed)));
        }
        let Row::Lease(mut row) = lease(99) else {
            panic!("expected fixture lease");
        };
        row.account = AccountId::from_bytes([99; 16]);
        assert!(matches!(
            ReferenceCheck::new(
                identity(),
                Key::Lease(UPLOAD),
                Row::Lease(row),
                Sequence::from_u64(1),
                100
            ),
            Err(Error::Format(_))
        ));
    }
    #[test]
    fn lookup_errors_wrong_rows_future_sequences_and_view_movement_retire() {
        for mode in 0..7 {
            let mut view = View::new();
            let (key, row) = if mode == 6 {
                (Key::Keyword(EMAIL, "$seen"), Row::Keyword)
            } else {
                (Key::Email(EMAIL), email())
            };
            let mut check =
                ReferenceCheck::new(identity(), key, row, Sequence::from_u64(2), 100).unwrap();
            match mode {
                0 => view.error = Some(ports::Error::Capacity),
                1 => view.rows[0].1 = Row::Thread,
                2 => view.sequence = Sequence::from_u64(3),
                3 => view.identity.epoch = crate::ids::StoreEpoch::from_bytes([99; 16]),
                4 => {
                    view.moved = true;
                    view.missing = true;
                }
                5 => {
                    view.moved = true;
                    view.error = Some(ports::Error::Corrupt);
                }
                _ => view.moved = true,
            }
            let error = check.advance(&mut view, &mut [0; 512]).unwrap_err();
            match mode {
                0 => assert_eq!(error, Error::View(ports::Error::Capacity)),
                1 | 2 => assert_eq!(error, Error::Format(format::Error::InvalidValue)),
                _ => assert_eq!(error, Error::ChangedView),
            }
            let calls = view.calls;
            assert_eq!(calls, usize::from(mode != 3));
            assert!(check.is_failed());
            assert!(!check.is_complete());
            assert_eq!(check.advance(&mut view, &mut [0; 512]), Err(Error::Failed));
            assert_eq!(view.calls, calls);
            assert!(matches!(check.finish(), Err(Error::Failed)));
        }
    }
    #[test]
    fn second_target_failure_retires_successful_partial_progress() {
        for mode in 0..4 {
            let mut view = View::new();
            let mut check = ReferenceCheck::new(
                identity(),
                Key::Email(EMAIL),
                email(),
                Sequence::from_u64(2),
                100,
            )
            .unwrap();
            assert!(!check.advance(&mut view, &mut [0; 512]).unwrap());
            assert_eq!(view.calls, 1);
            match mode {
                0 => view.missing = true,
                1 => view.identity.epoch = crate::ids::StoreEpoch::from_bytes([99; 16]),
                2 => view.rows[3].1 = Row::Keyword,
                _ => view.error = Some(ports::Error::Capacity),
            }
            let error = check.advance(&mut view, &mut [0; 512]).unwrap_err();
            match mode {
                0 => assert_eq!(error, Error::Missing(Target::Thread(THREAD))),
                1 => assert_eq!(error, Error::ChangedView),
                2 => assert_eq!(error, Error::Format(format::Error::InvalidValue)),
                _ => assert_eq!(error, Error::View(ports::Error::Capacity)),
            }
            assert_eq!(view.calls, if mode == 1 { 1 } else { 2 });
            assert!(!check.is_complete());
            assert!(check.is_failed());
            assert_eq!(check.advance(&mut view, &mut [0; 512]), Err(Error::Failed));
            assert!(matches!(check.finish(), Err(Error::Failed)));
        }
    }
    #[test]
    fn malformed_source_and_unfinished_checks_never_complete() {
        assert!(matches!(
            ReferenceCheck::new(
                identity(),
                Key::Email(EMAIL),
                Row::Thread,
                Sequence::from_u64(1),
                100
            ),
            Err(Error::Format(_))
        ));
        assert!(matches!(
            ReferenceCheck::new(
                identity(),
                Key::Email(EMAIL),
                email(),
                Sequence::from_u64(3),
                100
            ),
            Err(Error::Format(_))
        ));
        for row in [email(), Row::Thread] {
            let key = if matches!(row, Row::Thread) {
                Key::Thread(THREAD)
            } else {
                Key::Email(EMAIL)
            };
            let check =
                ReferenceCheck::new(identity(), key, row, Sequence::from_u64(1), 100).unwrap();
            assert!(matches!(check.finish(), Err(Error::Incomplete)));
        }
        let mut view = View::new();
        let mut check = ReferenceCheck::new(
            identity(),
            Key::Thread(THREAD),
            Row::Thread,
            Sequence::from_u64(1),
            100,
        )
        .unwrap();
        assert!(check.advance(&mut view, &mut []).unwrap());
        view.identity.epoch = crate::ids::StoreEpoch::from_bytes([99; 16]);
        assert_eq!(check.advance(&mut view, &mut []), Err(Error::ChangedView));
        assert!(matches!(check.finish(), Err(Error::Failed)));
    }
}
