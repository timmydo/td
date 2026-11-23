//! td's synchronous PostgreSQL client. DESIGN.md is normative: it says
//! what td-mta needs of a client, which of that this crate covers, and
//! what each bound and error means.
//!
//! One `Client` owns one connection and blocks its caller. Every
//! operation takes a deadline, every server byte is untrusted and bounded
//! by the caller's `Limits`, and any failure that leaves the protocol's
//! state unknown closes the connection for good.

mod client;
mod rows;
mod scram;
mod stream;
mod types;
mod wire;

pub use client::{connect_unix, connect_unix_through, startup, Client, Notification};
pub use rows::{Row, RowRef, Rows};
pub use stream::{Socket, Stream, StreamError};
pub use types::{oid, Array, Decode, DecodeError, Element, Encode, Typed};

use std::fmt;

/// Why an operation failed. Every variant but `Db` with an `ERROR`
/// severity, `RowCount` and the refusals made before anything is sent
/// (`Invalid`, and `Allocation` or `Limit` while a request is built) leaves
/// the client broken: its later operations return `Broken`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The caller's input was refused before anything was sent.
    Invalid,
    /// td-alloc refused a reservation.
    Allocation,
    /// The server exceeded one of the caller's `Limits`, or a request
    /// would have.
    Limit,
    /// The deadline passed.
    Deadline,
    /// The server closed the connection.
    Closed,
    /// The transport failed.
    Io,
    /// The server broke the protocol.
    Protocol,
    /// Authentication was refused: the server offered no SCRAM-SHA-256,
    /// asked for another method, or failed to prove it knows the password.
    Authentication,
    /// The server reported an error.
    Db(DbError),
    /// A query meant to return one row, or at most one, returned more or
    /// none. The connection stays usable.
    RowCount,
    /// An earlier failure closed this connection.
    Broken,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid => f.write_str("invalid PostgreSQL request"),
            Self::Allocation => f.write_str("PostgreSQL client allocation refused"),
            Self::Limit => f.write_str("PostgreSQL client limit exceeded"),
            Self::Deadline => f.write_str("PostgreSQL deadline passed"),
            Self::Closed => f.write_str("PostgreSQL server closed the connection"),
            Self::Io => f.write_str("PostgreSQL transport failed"),
            Self::Protocol => f.write_str("PostgreSQL protocol violation"),
            Self::Authentication => f.write_str("PostgreSQL authentication refused"),
            Self::Db(error) => write!(f, "PostgreSQL error {}", error.code()),
            Self::RowCount => f.write_str("unexpected PostgreSQL row count"),
            Self::Broken => f.write_str("PostgreSQL connection is broken"),
        }
    }
}

impl std::error::Error for Error {}

impl From<td_alloc::Error> for Error {
    fn from(_: td_alloc::Error) -> Self {
        Self::Allocation
    }
}

/// A server error: its severity and SQLSTATE only. The message, detail
/// and hint are not kept, since they can quote the data a statement
/// carried and every diagnostic td-mta makes is redacted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DbError {
    severity: Severity,
    code: SqlState,
}

impl DbError {
    pub(crate) fn new(severity: Severity, code: SqlState) -> Self {
        Self { severity, code }
    }

    pub fn severity(&self) -> Severity {
        self.severity
    }

    pub fn code(&self) -> SqlState {
        self.code
    }
}

/// An `ErrorResponse`'s severity. `Error` ends the statement and leaves
/// the session; `Fatal` and `Panic` end the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Fatal,
    Panic,
}

/// A five-character SQLSTATE, digits and uppercase letters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SqlState([u8; 5]);

impl SqlState {
    /// `55P03`, which `lock_timeout` and `NOWAIT` raise.
    pub const LOCK_NOT_AVAILABLE: Self = Self(*b"55P03");

    /// The code, if `code` is five digits or uppercase ASCII letters.
    pub fn new(code: &[u8; 5]) -> Option<Self> {
        code.iter()
            .all(|byte| byte.is_ascii_digit() || byte.is_ascii_uppercase())
            .then_some(Self(*code))
    }

    pub fn as_str(&self) -> &str {
        // new() admits only ASCII.
        std::str::from_utf8(&self.0).unwrap_or("?????")
    }
}

impl fmt::Display for SqlState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What `Client::commit` reports when it cannot say the transaction
/// committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitError {
    /// COMMIT was never written, so the transaction did not commit: the
    /// server rolls back an open transaction when its connection ends.
    NotSent(Error),
    /// The server answered COMMIT and the transaction did not commit:
    /// with an `ERROR` before anything completed, or with a `ROLLBACK` tag
    /// for a transaction that had already failed. An `ERROR` that leaves
    /// the block failed also breaks the client.
    RolledBack(Option<DbError>),
    /// COMMIT was written, and no complete answer that ends in
    /// ReadyForQuery came back: the transaction may or may not have
    /// committed. The client is broken.
    OutcomeUnknown(Error),
}

impl fmt::Display for CommitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSent(error) => write!(f, "COMMIT not sent: {error}"),
            Self::RolledBack(_) => f.write_str("PostgreSQL transaction rolled back"),
            Self::OutcomeUnknown(error) => write!(f, "COMMIT outcome unknown: {error}"),
        }
    }
}

impl std::error::Error for CommitError {}

/// The transaction state the server last reported in ReadyForQuery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransactionStatus {
    Idle,
    InTransaction,
    Failed,
}

/// The caller's bounds on what one connection takes from its server.
/// Every count and size here is the caller's choice; nothing defaults.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The largest backend message body accepted, in bytes, at least 256.
    /// The read buffer grows to this and no further.
    pub message_bytes: usize,
    /// The largest request one operation may write, in bytes, at least 64.
    pub request_bytes: usize,
    /// The most backend messages one operation, the startup, or one idle
    /// wait may read, data rows and asynchronous messages included.
    pub messages: usize,
    /// The most rows one query may return.
    pub rows: usize,
    /// The most columns one result may have, at most 1664.
    pub columns: usize,
    /// The most bytes one query's retained result may hold: column names,
    /// values, and 16 bytes of bookkeeping a column and 8 a value. At most
    /// `u32::MAX`.
    pub result_bytes: usize,
    /// The most notifications held for the caller; later ones are counted
    /// and dropped.
    pub notifications: usize,
    /// The most SCRAM iterations the server may ask for, at least 1.
    pub scram_iterations: u32,
}

impl Limits {
    pub(crate) fn check(&self) -> Result<(), Error> {
        let valid = self.message_bytes >= 256
            && self.message_bytes <= wire::MAX_MESSAGE_BYTES
            && self.request_bytes >= 64
            && self.request_bytes <= wire::MAX_MESSAGE_BYTES
            && self.messages >= 1
            && self.columns <= 1664
            && u32::try_from(self.result_bytes).is_ok()
            && self.scram_iterations >= 1;
        if valid {
            Ok(())
        } else {
            Err(Error::Invalid)
        }
    }
}

/// What the startup message carries. `parameters` are further run-time
/// settings sent with it, such as `("options", "-c search_path=pg_catalog")`
/// or `("statement_timeout", "5000")`; the client sets `user`, `database`,
/// `application_name` and `client_encoding` itself and refuses those, and
/// `replication` and protocol options, there.
#[derive(Clone, Copy, Debug)]
pub struct Config<'a> {
    pub user: &'a str,
    pub database: &'a str,
    /// ASCII only, without NUL: SASLprep leaves such a password as it is,
    /// and this client does not implement SASLprep for any other.
    pub password: &'a [u8],
    pub application_name: &'a str,
    pub parameters: &'a [(&'a str, &'a str)],
}
