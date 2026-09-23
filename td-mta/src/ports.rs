//! Synchronous adapter contracts; runtime workers own scheduling and pool leases.
//! No adapter implementation or durability claim is supplied by these traits.
use crate::{
    format::{
        key::Key,
        row::{BlobKind, Row},
        ObjectType, Sequence, Table,
    },
    ids::{AccountId, BlobId, DeviceId, StoreEpoch},
};
use std::{io::ErrorKind, net::IpAddr};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Capacity,
    Quota,
    Busy,
    Deadline,
    NotFound,
    Forbidden,
    Invalid,
    Corrupt,
    Conflict,
    HistoryLost,
    WriterStopped,
    Entropy,
    Crypto,
    Tls,
    Dns,
    Io {
        kind: ErrorKind,
        os_code: Option<i32>,
    },
}
impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Io {
            kind: error.kind(),
            os_code: error.raw_os_error(),
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "adapter failure: {self:?}")
    }
}
impl std::error::Error for Error {}

/// Monotonic milliseconds within one boot; never a persisted UTC timestamp.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Tick(pub u64);
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Deadline(Tick);
impl Deadline {
    pub fn after(now: Tick, milliseconds: u64) -> Result<Self, Error> {
        now.0
            .checked_add(milliseconds)
            .map(|v| Self(Tick(v)))
            .ok_or(Error::Invalid)
    }
    pub const fn tick(self) -> Tick {
        self.0
    }
    pub fn expired(self, now: Tick) -> bool {
        now >= self.0
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Time {
    pub utc_ms: i64,
    pub monotonic: Tick,
}
pub trait Clock: Send + Sync {
    fn sample(&self) -> Result<Time, Error>;
}
pub trait Entropy {
    fn fill(&mut self, output: &mut [u8]) -> Result<(), Error>;
}
pub trait Digest: Send {
    fn update(&mut self, bytes: &[u8]) -> Result<(), Error>;
    fn finish(self) -> Result<[u8; 32], Error>;
}
pub trait Crypto: Send + Sync {
    type Sha256: Digest;
    type SigningKey: Send + Sync;
    fn sha256(&self) -> Self::Sha256;
    /// Provider-backed constant-time equality for fixed-size password verifiers.
    fn equal_digest(&self, left: &[u8; 32], right: &[u8; 32]) -> bool;
    /// Cold path only; output is a complete PKCS#8 P-256 private key.
    fn generate_p256(&self, output: &mut [u8]) -> Result<usize, Error>;
    /// Cold path only; a key's owned provider storage counts in the cold ledger.
    fn load_p256(&self, pkcs8: &[u8]) -> Result<Self::SigningKey, Error>;
    /// Uncompressed SEC1 point: 0x04 followed by fixed-width X and Y.
    fn p256_public(&self, key: &Self::SigningKey, output: &mut [u8; 65]) -> Result<(), Error>;
    /// SHA-256 ECDSA signature in fixed-width r||s form; DER wrapping is core code.
    fn sign_es256(
        &self,
        key: &Self::SigningKey,
        message: &[u8],
        output: &mut [u8; 64],
    ) -> Result<(), Error>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IoProgress {
    Bytes(usize),
    Pending,
    Closed,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlushProgress {
    Pending,
    Complete,
}
/// Nonblocking operation. Bytes(n) has 1 <= n <= slice.len(); empty input
/// returns Pending. Closed is EOF on read; write failure is an Error.
pub trait Transport: Send {
    fn read(&mut self, output: &mut [u8]) -> Result<IoProgress, Error>;
    fn write(&mut self, input: &[u8]) -> Result<IoProgress, Error>;
    /// Drain adapter-owned output (including TLS records), not peer receipt.
    fn flush(&mut self) -> Result<FlushProgress, Error>;
    /// Send orderly closure, including close_notify; may need more progress.
    fn close(&mut self) -> Result<FlushProgress, Error>;
    /// Immediately fence I/O and drop buffered output; cancellation uses this.
    fn abort(&mut self);
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TlsVersion {
    V12,
    V13,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeerVerification {
    /// Public inbound TLS does not authenticate a remote SMTP identity.
    None,
    /// Chain, validity and configured server name were all verified.
    ServerName,
    /// Certificate identity verified against the configured gateway trust root.
    Gateway([u8; 32]),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TlsInfo {
    pub version: TlsVersion,
    pub peer: PeerVerification,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Handshake {
    Pending,
    Complete(TlsInfo),
}
pub trait TlsTransport: Transport {
    fn handshake(&mut self, deadline: Deadline) -> Result<Handshake, Error>;
    /// None until a successful handshake; credentials require the right peer proof.
    fn info(&self) -> Option<TlsInfo>;
}
/// Cold-registered policy, never a remote-supplied trust setting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TlsPolicyId {
    pub generation: u64,
    pub index: u16,
}
pub trait TlsFactory: Send {
    type Plain: Transport;
    type Secure: TlsTransport;
    /// Consumes and closes plaintext on refusal. Unconsumed plaintext must be
    /// empty; the caller completes STARTTLS framing before handing off.
    fn upgrade(
        &mut self,
        plain: Self::Plain,
        unconsumed_plaintext: &[u8],
        policy: TlsPolicyId,
        deadline: Deadline,
    ) -> Result<Self::Secure, Error>;
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Resolution {
    pub count: usize,
    pub valid_until: Tick,
}
/// Runs only on the fixed resolver worker. Caller supplies address capacity.
pub trait Resolver {
    fn resolve(
        &mut self,
        host: &str,
        deadline: Deadline,
        output: &mut [IpAddr],
    ) -> Result<Resolution, Error>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ViewIdentity {
    pub account: AccountId,
    pub epoch: StoreEpoch,
    pub generation: u64,
    pub checkpoint: Sequence,
    pub segment: u64,
    pub committed_offset: u64,
    pub committed_sequence: Sequence,
    pub history_floor: Sequence,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Record<'a> {
    pub key: Key<'a>,
    pub row: Row<'a>,
    pub last_change: Sequence,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChangeCursor {
    pub sequence: Sequence,
    /// Zero-based operation in a frame; u32::MAX skips its whole sequence.
    pub operation: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChangeRecord {
    pub cursor: ChangeCursor,
    pub change: Change,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeStep {
    Record(ChangeRecord),
    /// Completed this whole frame, including when no matching CHANGE existed.
    Advanced {
        through: Sequence,
    },
    Complete,
}
pub trait ReadView: Send + Sync {
    fn identity(&self) -> ViewIdentity;
    /// Scan strictly after the cursor, within the pinned history and endpoint.
    /// Body PUTs are skipped with bounded I/O; no whole-frame allocation.
    fn next_change(&mut self, after: ChangeCursor, kind: ObjectType) -> Result<ChangeStep, Error>;
    /// Decode and validate into caller storage; None means absent in this view.
    fn get<'a>(
        &mut self,
        key: Key<'_>,
        value: &'a mut [u8],
    ) -> Result<Option<(Row<'a>, Sequence)>, Error>;
    /// Strictly after the encoded key (or first); ascending canonical key order.
    fn next<'a>(
        &mut self,
        table: Table,
        after: Option<&[u8]>,
        key: &'a mut [u8],
        value: &'a mut [u8],
    ) -> Result<Option<Record<'a>>, Error>;
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mutation<'a> {
    Put { key: Key<'a>, row: Row<'a> },
    Delete(Key<'a>),
}
/// Canonical FORMAT PUT/DELETE/CHANGE operations, without a frame header.
/// The store validates count, framing, rows and references before any append.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransactionInput<'a> {
    pub bytes: &'a [u8],
    pub count: usize,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationKind {
    Put,
    Delete,
    Change(ChangeAction),
}
/// Owned offsets into TransactionInput, never references into a reusable arena.
/// All ranges are validated by the store; Rust layout is not serialized.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StagedOperation {
    pub kind: OperationKind,
    pub type_tag: u16,
    pub key_offset: u32,
    pub key_len: u32,
    pub value_offset: u32,
    pub value_len: u32,
    pub ordinal: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeAction {
    Created,
    Updated,
    Destroyed,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Change {
    pub kind: ObjectType,
    pub id: [u8; 16],
    pub action: ChangeAction,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Principal {
    Smtp,
    Device(DeviceId),
    Queue,
    Administrator,
    Maintenance,
    Migration,
}
/// Trusted coordinator context, not an authentication credential.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Access {
    pub account: AccountId,
    pub principal: Principal,
    pub config_generation: u64,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DiskBudget {
    pub new_blob_bytes: u64,
    pub new_blob_files: u32,
    /// Logical quota increases, including pins on an already existing blob.
    pub upload_bytes: u64,
    pub queue_bytes: u64,
    pub metadata_bytes: u64,
    pub metadata_files: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReservationSize {
    pub frame_bytes: usize,
    pub operations: usize,
    pub disk: DiskBudget,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReservationId {
    pub coordinator: [u8; 16],
    pub slot: u32,
    pub generation: u64,
}
pub trait Reservation: Send {
    fn identity(&self) -> ReservationId;
    fn account(&self) -> AccountId;
    fn size(&self) -> ReservationSize;
    fn deadline(&self) -> Deadline;
    fn is_active(&self) -> bool;
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Commit {
    pub account: AccountId,
    pub epoch: StoreEpoch,
    pub sequence: Sequence,
}
/// Rejected guarantees this attempt appended no frame. Indeterminate means a
/// recoverable frame may exist: stop the writer and recover before more writes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitFailure {
    Rejected(Error),
    Indeterminate(Error),
}
impl std::fmt::Display for CommitFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(error) => write!(f, "commit rejected: {error}"),
            Self::Indeterminate(error) => write!(f, "commit outcome indeterminate: {error}"),
        }
    }
}
impl std::error::Error for CommitFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Rejected(error) | Self::Indeterminate(error) => Some(error),
        }
    }
}
/// The writer worker alone calls commit; frontends submit fixed-slot requests.
/// Associated handles lease startup pools; Drop releases their resources.
pub trait Store: Send + Sync {
    type View<'a>: ReadView
    where
        Self: 'a;
    type Reserved<'a>: Reservation
    where
        Self: 'a;
    type Reader<'a>: BlobReader
    where
        Self: 'a;
    type Writer<'a>: BlobWriter
    where
        Self: 'a;
    fn read_view(&self, access: Access, deadline: Deadline) -> Result<Self::View<'_>, Error>;
    /// Keeps the view/lease pin for this authorized root blob until drop.
    fn open_blob<'a>(
        &'a self,
        access: Access,
        view: &'a Self::View<'_>,
        id: BlobId,
    ) -> Result<Self::Reader<'a>, Error>;
    /// Charges the reservation before exclusive creation; the writer borrows
    /// it until publication/drop, preventing concurrent use by a commit.
    fn begin_blob<'a>(
        &'a self,
        reservation: &'a mut Self::Reserved<'_>,
        id: BlobId,
        kind: BlobKind,
    ) -> Result<Self::Writer<'a>, Error>;
    /// Includes retained CHANGE bytes/count; fails before a new body is admitted.
    fn reserve(
        &self,
        access: Access,
        size: ReservationSize,
        deadline: Deadline,
    ) -> Result<Self::Reserved<'_>, Error>;
    /// Ok consumes capacity; Conflict retains the active reservation for a
    /// bounded replan. Indeterminate poisons it and stops the writer.
    fn commit(
        &self,
        access: Access,
        reservation: &mut Self::Reserved<'_>,
        expected: Sequence,
        published: &[PublishedBlob],
        transaction: TransactionInput<'_>,
    ) -> Result<Commit, CommitFailure>;
}
/// The owning view/lease remains live while this reader is used.
pub trait BlobReader: Send {
    fn len(&self) -> u64;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<usize, Error>;
}
#[derive(Debug, Eq, PartialEq)]
pub struct PublishedBlob {
    pub(crate) reservation: ReservationId,
    pub(crate) account: AccountId,
    pub(crate) id: BlobId,
    pub(crate) length: u64,
    pub(crate) digest: [u8; 32],
}
impl PublishedBlob {
    pub const fn reservation(&self) -> ReservationId {
        self.reservation
    }
    pub const fn account(&self) -> AccountId {
        self.account
    }
    pub const fn id(&self) -> BlobId {
        self.id
    }
    pub const fn length(&self) -> u64 {
        self.length
    }
    pub const fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
}
pub trait BlobWriter: Send {
    /// All-or-error: no caller retry of the same chunk after an error.
    fn write(&mut self, bytes: &[u8]) -> Result<(), Error>;
    /// Sync file and publication directories; does not create a metadata reference.
    fn publish(self) -> Result<PublishedBlob, Error>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::row::{
        AttemptPhase, FailureReason, NotificationState, RecipientRow, RecipientState,
        SubmissionRow, MAX_ADDRESS, MAX_DIAGNOSTIC, MAX_SMTP_REPLY,
    };
    use crate::format::{
        FRAME_FOOTER_BYTES, FRAME_HEADER_BYTES, MAX_FRAME_BYTES, MAX_FRAME_OPERATIONS,
        OPERATION_HEADER_BYTES,
    };
    use crate::ids::{AttemptId, EmailId, IdentityId, SubmissionId, ThreadId};

    #[test]
    fn monotonic_deadlines_expire_at_boundary_and_refuse_overflow() -> Result<(), Error> {
        let deadline = Deadline::after(Tick(10), 5)?;
        assert_eq!(deadline.tick(), Tick(15));
        assert!(!deadline.expired(Tick(14)));
        assert!(deadline.expired(Tick(15)));
        assert!(Deadline::after(Tick(10), 0)?.expired(Tick(10)));
        assert_eq!(Deadline::after(Tick(u64::MAX), 1), Err(Error::Invalid));
        assert_eq!(Deadline::after(Tick(u64::MAX), 0)?.tick(), Tick(u64::MAX));
        Ok(())
    }

    #[test]
    fn recipient_batch_creation_and_cancellation_have_explicit_frame_bounds(
    ) -> Result<(), crate::format::Error> {
        let address = "a".repeat(MAX_ADDRESS);
        let reply = "2".repeat(MAX_SMTP_REPLY);
        let diagnostic = "x".repeat(MAX_DIAGNOSTIC);
        let recipient = RecipientRow {
            address: &address,
            state: RecipientState::RetryWait,
            uncertain: false,
            attempt: Some(AttemptId::from_bytes([1; 16])),
            attempt_count: 1,
            last_attempt_at: Some(1),
            phase: AttemptPhase::Final,
            next_attempt_at: Some(2),
            rcpt_reply: Some(&reply),
            data_reply: Some(&reply),
            reason: FailureReason::SmtpTemporary,
            diagnostic: &diagnostic,
        };
        // Field-size proof only: these strings are not a valid SMTP transcript.
        let row_bytes = Row::Recipient(recipient).encoded_len()?;
        assert_eq!(row_bytes, 9019);
        let id = SubmissionId::from_bytes([1; 16]);
        let recipient_overhead = OPERATION_HEADER_BYTES + Key::Recipient(id, 0).encoded_len()?;
        let recipient_puts = (row_bytes + recipient_overhead) * 100;
        assert_eq!(recipient_puts, 905100);
        let submission_bytes = Row::Submission(SubmissionRow {
            email: EmailId::from_bytes([1; 16]),
            thread: ThreadId::from_bytes([2; 16]),
            identity: IdentityId::from_bytes([3; 16]),
            transmitted_blob: BlobId::from_bytes([4; 16]),
            reverse_path: &address,
            send_at: 0,
            expires_at: 432_000_000,
            recipient_count: 1000,
            completed_at: Some(2),
            notification: NotificationState::Stored,
            notification_email: Some(EmailId::from_bytes([5; 16])),
        })
        .encoded_len()?;
        let envelope = FRAME_HEADER_BYTES
            + FRAME_FOOTER_BYTES
            + OPERATION_HEADER_BYTES
            + Key::Submission(id).encoded_len()?
            + submission_bytes
            + OPERATION_HEADER_BYTES
            + 16; // CHANGE key, no value.
        assert!(recipient_puts + envelope < MAX_FRAME_BYTES);
        let canceled = RecipientRow {
            state: RecipientState::Canceled,
            next_attempt_at: None,
            reason: FailureReason::Canceled,
            ..recipient
        };
        let long_cancel = Row::Recipient(canceled).encoded_len()? + recipient_overhead;
        assert!(long_cancel * 100 + envelope < MAX_FRAME_BYTES);
        assert!(long_cancel * 1000 + envelope > MAX_FRAME_BYTES);
        let queued = RecipientRow {
            state: RecipientState::Queued,
            attempt: None,
            attempt_count: 0,
            last_attempt_at: None,
            phase: AttemptPhase::None,
            rcpt_reply: None,
            data_reply: None,
            reason: FailureReason::None,
            diagnostic: "",
            ..recipient
        };
        let fresh_cancel = RecipientRow {
            state: RecipientState::Canceled,
            next_attempt_at: None,
            reason: FailureReason::Canceled,
            ..queued
        };
        let blob_id = BlobId::from_bytes([4; 16]);
        let blob_put = OPERATION_HEADER_BYTES
            + Key::Blob(blob_id).encoded_len()?
            + Row::Blob(crate::format::row::BlobRow {
                kind: BlobKind::Message,
                length: 1,
                digest: [0; 32],
                created_at: 0,
            })
            .encoded_len()?;
        for row in [queued, fresh_cancel] {
            let bytes = (Row::Recipient(row).encoded_len()? + recipient_overhead) * 1000;
            // Creation also adds the transmitted blob row.
            assert!(bytes + envelope + blob_put < MAX_FRAME_BYTES);
        }
        const { assert!(1000 + 3 < MAX_FRAME_OPERATIONS) };
        Ok(())
    }
}
