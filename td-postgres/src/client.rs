//! One connection: startup and SCRAM, the simple and extended queries,
//! transactions with an explicit unknown COMMIT outcome, and
//! notifications.
use crate::rows::{Row, Rows};
use crate::scram::{self, Exchange};
use crate::stream::{remaining, Socket, Stream, StreamError};
use crate::types::Encode;
use crate::wire::{self, Authentication, Backend, Inbox};
use crate::{CommitError, Config, DbError, Error, Limits, Severity, TransactionStatus};
use std::collections::VecDeque;
use std::path::Path;
use std::time::{Duration, Instant};

/// How long dropping a healthy client may spend writing Terminate.
const TERMINATE_WAIT: Duration = Duration::from_millis(100);
/// Startup parameters the client sets, or that would change what the
/// session is: a replication connection speaks another protocol.
const RESERVED: &[&str] = &[
    "user",
    "database",
    "application_name",
    "client_encoding",
    "replication",
];

/// A NotificationResponse: a NOTIFY on a channel this session LISTENs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    process_id: i32,
    channel: String,
    payload: String,
}

impl Notification {
    /// The notifying backend's process ID.
    pub fn process_id(&self) -> i32 {
        self.process_id
    }

    pub fn channel(&self) -> &str {
        &self.channel
    }

    pub fn payload(&self) -> &str {
        &self.payload
    }
}

/// A connection. Every operation takes `&mut self` and blocks.
pub struct Client<S: Stream> {
    stream: S,
    inbox: Inbox,
    out: Vec<u8>,
    limits: Limits,
    status: TransactionStatus,
    /// A request is written and its ReadyForQuery has not arrived.
    pending: bool,
    broken: bool,
    /// The server has reported `client_encoding` as UTF8.
    utf8: bool,
    /// AuthenticationOk has been accepted: before it, only notices may
    /// arrive between the login's own messages.
    authenticated: bool,
    notifications: VecDeque<Notification>,
    dropped: u64,
}

/// What ended a simple query's last statement before its
/// ReadyForQuery, or before its ERROR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tag {
    /// Nothing: no statement completed.
    None,
    Empty,
    Commit,
    Rollback,
    Other,
    /// More than one statement completed.
    Several,
}

impl Tag {
    /// The tag after one more completion.
    fn then(self, next: Tag) -> Tag {
        if self == Tag::None {
            next
        } else {
            Tag::Several
        }
    }
}

/// The largest NOTIFY channel (NAMEDATALEN - 1) and payload (below
/// 8000 bytes) a default-built PostgreSQL sends. A larger notification
/// is counted and dropped, so held ones stay small.
const NOTIFY_CHANNEL_BYTES: usize = 63;
const NOTIFY_PAYLOAD_BYTES: usize = 7999;

/// What one extended query keeps of its rows.
enum Keep<'a> {
    Rows(&'a mut Rows),
    Count,
}

/// Connect to a Unix-domain socket and start a session. The socket's
/// directory and ownership are the caller's to check before this.
pub fn connect_unix(
    path: &Path,
    config: &Config<'_>,
    limits: Limits,
    deadline: Instant,
) -> Result<Client<Socket>, Error> {
    connect_unix_through(path, |socket| socket, config, limits, deadline)
}

/// `connect_unix`, with the session run over the stream `wrap` makes of
/// the connected socket: a caller's own byte budget or input filter,
/// which then sees every byte of the startup and the login too.
pub fn connect_unix_through<W: Stream>(
    path: &Path,
    wrap: impl FnOnce(Socket) -> W,
    config: &Config<'_>,
    limits: Limits,
    deadline: Instant,
) -> Result<Client<W>, Error> {
    check(config, &limits)?;
    remaining(deadline).ok_or(Error::Deadline)?;
    let socket = td_fs::unix_connect_until(path, deadline).map_err(io)?;
    startup(wrap(Socket::new(socket)), config, limits, deadline)
}

/// Start a session over a stream the caller connected: the startup
/// message, SCRAM-SHA-256, and the server's parameters up to its first
/// ReadyForQuery.
pub fn startup<S: Stream>(
    stream: S,
    config: &Config<'_>,
    limits: Limits,
    deadline: Instant,
) -> Result<Client<S>, Error> {
    check(config, &limits)?;
    let mut exchange = Exchange::new(b"", config.password)?;
    Client::start(stream, config, limits, deadline, &mut exchange)
}

fn io(error: std::io::Error) -> Error {
    if error.kind() == std::io::ErrorKind::TimedOut {
        Error::Deadline
    } else {
        Error::Io
    }
}

fn stream_error(error: StreamError) -> Error {
    match error {
        StreamError::TimedOut => Error::Deadline,
        StreamError::Failed => Error::Io,
    }
}

fn check(config: &Config<'_>, limits: &Limits) -> Result<(), Error> {
    limits.check()?;
    let text = |value: &str| !value.as_bytes().contains(&0);
    let password = config.password;
    let valid = !config.user.is_empty()
        && !config.database.is_empty()
        && text(config.user)
        && text(config.database)
        && text(config.application_name)
        && !password.is_empty()
        && password.iter().all(|&byte| byte != 0 && byte.is_ascii());
    if !valid {
        return Err(Error::Invalid);
    }
    for (at, (name, value)) in config.parameters.iter().enumerate() {
        let repeated = config
            .parameters
            .get(..at)
            .is_some_and(|before| before.iter().any(|(other, _)| other == name));
        if name.is_empty()
            || !text(name)
            || !text(value)
            || RESERVED.contains(name)
            || name.starts_with("_pq_.")
            || repeated
        {
            return Err(Error::Invalid);
        }
    }
    Ok(())
}

/// The rows a CommandComplete tag reports, 0 for a command that reports
/// none. A counting command whose count does not parse is refused.
fn affected(tag: &[u8]) -> Result<u64, Error> {
    let mut words = tag.split(|&byte| byte == b' ');
    match words.next() {
        Some(b"INSERT") => {
            // The OID word, always 0 since PostgreSQL 12.
            words.next().ok_or(Error::Protocol)?;
        }
        Some(b"SELECT" | b"UPDATE" | b"DELETE" | b"MERGE" | b"FETCH" | b"MOVE" | b"COPY") => {}
        _ => return Ok(0),
    }
    let count = words.next().ok_or(Error::Protocol)?;
    if words.next().is_some() || count.is_empty() || !count.iter().all(u8::is_ascii_digit) {
        return Err(Error::Protocol);
    }
    std::str::from_utf8(count)
        .ok()
        .and_then(|count| count.parse().ok())
        .ok_or(Error::Protocol)
}

impl<S: Stream> Client<S> {
    fn start(
        stream: S,
        config: &Config<'_>,
        limits: Limits,
        deadline: Instant,
        exchange: &mut Exchange<'_>,
    ) -> Result<Self, Error> {
        let mut client = Self {
            stream,
            inbox: Inbox::default(),
            out: Vec::new(),
            limits,
            status: TransactionStatus::Idle,
            pending: true,
            broken: false,
            utf8: false,
            authenticated: false,
            notifications: VecDeque::new(),
            dropped: 0,
        };
        let result = client.authenticate(config, deadline, exchange);
        client.settle(result)?;
        Ok(client)
    }

    fn authenticate(
        &mut self,
        config: &Config<'_>,
        deadline: Instant,
        exchange: &mut Exchange<'_>,
    ) -> Result<(), Error> {
        let mut parameters: Vec<(&str, &str)> = Vec::new();
        td_alloc::reserve(&mut parameters, config.parameters.len().saturating_add(4))?;
        td_alloc::push(&mut parameters, ("user", config.user))?;
        td_alloc::push(&mut parameters, ("database", config.database))?;
        if !config.application_name.is_empty() {
            td_alloc::push(
                &mut parameters,
                ("application_name", config.application_name),
            )?;
        }
        td_alloc::push(&mut parameters, ("client_encoding", "UTF8"))?;
        for parameter in config.parameters {
            td_alloc::push(&mut parameters, *parameter)?;
        }
        wire::startup(&mut self.out, &parameters)?;
        self.send(deadline)?;

        #[derive(PartialEq)]
        enum Stage {
            Mechanisms,
            Continue,
            Final,
            Ok,
            Parameters,
        }
        let mut stage = Stage::Mechanisms;
        let mut budget = self.limits.messages;
        loop {
            let (tag, start, end) = self.next(deadline, &mut budget)?;
            let message = wire::backend(tag, self.inbox.body(start, end))?;
            match (&stage, message) {
                (Stage::Mechanisms, Backend::Authentication(Authentication::Sasl(list))) => {
                    if !list.offers(scram::MECHANISM) {
                        return Err(Error::Authentication);
                    }
                    let mut first = Vec::new();
                    exchange.client_first(&mut first)?;
                    wire::sasl_initial(&mut self.out, scram::MECHANISM, &first)?;
                    self.send(deadline)?;
                    stage = Stage::Continue;
                }
                (Stage::Continue, Backend::Authentication(Authentication::SaslContinue(data))) => {
                    let mut last = Vec::new();
                    exchange.client_final(data, self.limits.scram_iterations, &mut last)?;
                    wire::sasl_response(&mut self.out, &last)?;
                    self.send(deadline)?;
                    stage = Stage::Final;
                }
                (Stage::Final, Backend::Authentication(Authentication::SaslFinal(data))) => {
                    exchange.verify(data)?;
                    stage = Stage::Ok;
                }
                // AuthenticationOk only once the server has proved itself.
                (Stage::Ok, Backend::Authentication(Authentication::Ok)) => {
                    self.authenticated = true;
                    stage = Stage::Parameters;
                }
                (_, Backend::Authentication(_)) => return Err(Error::Authentication),
                (_, Backend::ErrorResponse(error)) => return Err(Error::Db(error)),
                (Stage::Parameters, Backend::KeyData) => {}
                (Stage::Parameters, Backend::ReadyForQuery(status)) => {
                    if !self.utf8 || status != TransactionStatus::Idle {
                        return Err(Error::Protocol);
                    }
                    self.status = status;
                    self.pending = false;
                    return Ok(());
                }
                _ => return Err(Error::Protocol),
            }
        }
    }

    /// Mark the client broken when an operation failed with its answer
    /// not wholly read.
    fn settle<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if result.is_err() && self.pending {
            self.broken = true;
        }
        result
    }

    /// Refuse an operation on a broken client or with a passed deadline.
    fn admit(&mut self, deadline: Instant) -> Result<(), Error> {
        if self.broken {
            return Err(Error::Broken);
        }
        self.out.clear();
        remaining(deadline).ok_or(Error::Deadline)?;
        Ok(())
    }

    /// Write the built request. Past its size check, it counts as sent.
    fn send(&mut self, deadline: Instant) -> Result<(), Error> {
        if self.out.len() > self.limits.request_bytes {
            self.out.clear();
            return Err(Error::Limit);
        }
        self.pending = true;
        let result = self.stream.write_all(&self.out, deadline);
        self.out.clear();
        match result {
            Ok(()) => Ok(()),
            Err(StreamError::Failed) => Err(self.ended(deadline).unwrap_or(Error::Io)),
            Err(error) => Err(stream_error(error)),
        }
    }

    /// After a failed write, the error the server ended the session with,
    /// if it is already here: a write to a session the server closed
    /// fails, and its FATAL says why. Only what is ready is read, within
    /// the operation's deadline and up to the first malformed message.
    fn ended(&mut self, deadline: Instant) -> Option<Error> {
        let mut budget = self.limits.messages;
        loop {
            remaining(deadline)?;
            let (tag, start, end) = self.receive(Instant::now(), &mut budget, true).ok()??;
            if let Backend::ErrorResponse(error) =
                wire::backend(tag, self.inbox.body(start, end)).ok()?
            {
                return (error.severity() != Severity::Error).then_some(Error::Db(error));
            }
        }
    }

    /// The next message an operation is answered with, past the
    /// asynchronous ones, each of which also counts against `budget`.
    fn next(&mut self, deadline: Instant, budget: &mut usize) -> Result<(u8, usize, usize), Error> {
        loop {
            let found = self.receive(deadline, budget, false)?;
            let (tag, start, end) = found.ok_or(Error::Deadline)?;
            if !self.asynchronous(tag, start, end)? {
                return Ok((tag, start, end));
            }
        }
    }

    fn receive(
        &mut self,
        deadline: Instant,
        budget: &mut usize,
        poll: bool,
    ) -> Result<Option<(u8, usize, usize)>, Error> {
        let found = self
            .inbox
            .next(&mut self.stream, self.limits.message_bytes, deadline, poll)?;
        if found.is_some() {
            *budget = budget.checked_sub(1).ok_or(Error::Limit)?;
        }
        Ok(found)
    }

    /// Handle a NoticeResponse, ParameterStatus or NotificationResponse,
    /// true when it was one. The client encoding may only be UTF8.
    fn asynchronous(&mut self, tag: u8, start: usize, end: usize) -> Result<bool, Error> {
        if !matches!(tag, b'N' | b'S' | b'A') {
            return Ok(false);
        }
        if tag != b'N' && !self.authenticated {
            return Err(Error::Protocol);
        }
        match wire::backend(tag, self.inbox.body(start, end))? {
            Backend::ParameterStatus { name, value } if name == b"client_encoding" => {
                if value != b"UTF8" {
                    return Err(Error::Protocol);
                }
                self.utf8 = true;
            }
            Backend::Notification(notice) => {
                if self.notifications.len() >= self.limits.notifications
                    || notice.channel.len() > NOTIFY_CHANNEL_BYTES
                    || notice.payload.len() > NOTIFY_PAYLOAD_BYTES
                {
                    self.dropped = self.dropped.saturating_add(1);
                    return Ok(true);
                }
                // A refused copy drops the hint; the protocol is intact.
                let kept = td_alloc::string(notice.channel).and_then(|channel| {
                    Ok(Notification {
                        process_id: notice.process_id,
                        channel,
                        payload: td_alloc::string(notice.payload)?,
                    })
                });
                let pushed =
                    kept.and_then(|kept| td_alloc::push_back(&mut self.notifications, kept));
                if pushed.is_err() {
                    self.dropped = self.dropped.saturating_add(1);
                }
            }
            _ => {}
        }
        Ok(true)
    }

    /// The transaction state of the last ReadyForQuery.
    pub fn transaction_status(&self) -> TransactionStatus {
        self.status
    }

    /// Whether an earlier failure closed this connection.
    pub fn is_broken(&self) -> bool {
        self.broken
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Change the limits for later operations.
    pub fn set_limits(&mut self, limits: Limits) -> Result<(), Error> {
        limits.check()?;
        self.limits = limits;
        Ok(())
    }

    /// Run `sql` through the simple query protocol, which may hold several
    /// statements, and discard any rows. End a transaction with `commit`
    /// or `rollback`, never here, so that COMMIT's outcome is always told
    /// apart.
    pub fn batch(&mut self, sql: &str, deadline: Instant) -> Result<(), Error> {
        self.admit(deadline)?;
        let result = self.simple(sql, deadline).and_then(|answer| match answer {
            (_, Some(error)) => Err(Error::Db(error)),
            (_, None) => Ok(()),
        });
        self.settle(result)
    }

    /// Roll back the open transaction; outside one the server only warns.
    pub fn rollback(&mut self, deadline: Instant) -> Result<(), Error> {
        self.admit(deadline)?;
        let result = match self.simple("ROLLBACK", deadline) {
            Ok((Tag::Rollback, None)) if self.status == TransactionStatus::Idle => Ok(()),
            Ok((_, Some(error))) => Err(Error::Db(error)),
            Ok(_) => {
                self.broken = true;
                Err(Error::Protocol)
            }
            Err(error) => Err(error),
        };
        self.settle(result)
    }

    /// Commit the open transaction, telling a definite refusal apart from
    /// an outcome nobody can know from here.
    pub fn commit(&mut self, deadline: Instant) -> Result<(), CommitError> {
        if let Err(error) = self.admit(deadline) {
            return Err(CommitError::NotSent(error));
        }
        if self.status == TransactionStatus::Idle {
            return Err(CommitError::NotSent(Error::Invalid));
        }
        let result = self.simple("COMMIT", deadline);
        let outcome = match (result, self.status) {
            (Ok((Tag::Commit, None)), TransactionStatus::Idle) => Ok(()),
            (Ok((Tag::Rollback, None)), TransactionStatus::Idle) => {
                Err(CommitError::RolledBack(None))
            }
            // An ERROR before anything completed, read to a ReadyForQuery
            // outside a live transaction: the commit failed.
            (Ok((Tag::None, Some(error))), TransactionStatus::Idle) => {
                Err(CommitError::RolledBack(Some(error)))
            }
            // Still in a failed block: it ends uncommitted with the
            // connection, which is not handed back in that state.
            (Ok((Tag::None, Some(error))), TransactionStatus::Failed) => {
                self.broken = true;
                Err(CommitError::RolledBack(Some(error)))
            }
            // A COMMIT tag then an ERROR, a live transaction after COMMIT,
            // or any other answer: nothing it says can be relied on.
            (Ok(_), _) => {
                self.broken = true;
                Err(CommitError::OutcomeUnknown(Error::Protocol))
            }
            // Written, and its answer not read to ReadyForQuery.
            (Err(error), _) if self.pending => Err(CommitError::OutcomeUnknown(error)),
            // Refused while the request was built: nothing was written.
            (Err(error), _) => Err(CommitError::NotSent(error)),
        };
        if self.pending {
            self.broken = true;
        }
        outcome
    }

    /// One simple Query, read to its ReadyForQuery: what ended the last
    /// statement before it or before its ERROR, and that ERROR.
    fn simple(&mut self, sql: &str, deadline: Instant) -> Result<(Tag, Option<DbError>), Error> {
        wire::query(&mut self.out, sql)?;
        self.send(deadline)?;
        let mut budget = self.limits.messages;
        let mut rows = 0usize;
        let mut width: Option<usize> = None;
        let mut tag = Tag::None;
        let mut failed = None;
        loop {
            let (kind, start, end) = self.next(deadline, &mut budget)?;
            let message = wire::backend(kind, self.inbox.body(start, end))?;
            match message {
                Backend::ErrorResponse(error) if failed.is_none() => {
                    if error.severity() != Severity::Error {
                        return Err(Error::Db(error));
                    }
                    // The failed statement's rows, if any, end here.
                    width = None;
                    failed = Some(error);
                }
                // Every statement is answered: a bare ReadyForQuery is not.
                Backend::ReadyForQuery(status)
                    if width.is_none() && (failed.is_some() || tag != Tag::None) =>
                {
                    self.status = status;
                    self.pending = false;
                    return Ok((tag, failed));
                }
                _ if failed.is_some() => return Err(Error::Protocol),
                Backend::RowDescription(description) if width.is_none() => {
                    if description.len() > self.limits.columns {
                        return Err(Error::Limit);
                    }
                    width = Some(description.len());
                }
                Backend::DataRow(row) if width == Some(row.len()) => {
                    rows = rows.checked_add(1).ok_or(Error::Limit)?;
                    if rows > self.limits.rows {
                        return Err(Error::Limit);
                    }
                }
                Backend::CommandComplete(text) => {
                    width = None;
                    tag = tag.then(match text {
                        b"COMMIT" => Tag::Commit,
                        b"ROLLBACK" => Tag::Rollback,
                        _ => Tag::Other,
                    });
                }
                Backend::EmptyQueryResponse if width.is_none() => tag = tag.then(Tag::Empty),
                _ => return Err(Error::Protocol),
            }
        }
    }

    /// Run one statement through the extended protocol, its parameters
    /// typed and binary, and return every row.
    pub fn query(
        &mut self,
        sql: &str,
        parameters: &[&dyn Encode],
        deadline: Instant,
    ) -> Result<Rows, Error> {
        self.admit(deadline)?;
        let mut rows = Rows::default();
        let result = self.extended(sql, parameters, deadline, Keep::Rows(&mut rows));
        self.settle(result).map(|_| rows)
    }

    /// `query` for exactly one row; any other count is `RowCount`.
    pub fn query_one(
        &mut self,
        sql: &str,
        parameters: &[&dyn Encode],
        deadline: Instant,
    ) -> Result<Row, Error> {
        self.query_opt(sql, parameters, deadline)?
            .ok_or(Error::RowCount)
    }

    /// `query` for at most one row; more is `RowCount`.
    pub fn query_opt(
        &mut self,
        sql: &str,
        parameters: &[&dyn Encode],
        deadline: Instant,
    ) -> Result<Option<Row>, Error> {
        let rows = self.query(sql, parameters, deadline)?;
        match rows.len() {
            0 => Ok(None),
            1 => Ok(Some(Row(rows))),
            _ => Err(Error::RowCount),
        }
    }

    /// Run one statement through the extended protocol and return the
    /// rows its command tag reports; rows it returns are counted against
    /// the limits and dropped.
    pub fn execute(
        &mut self,
        sql: &str,
        parameters: &[&dyn Encode],
        deadline: Instant,
    ) -> Result<u64, Error> {
        self.admit(deadline)?;
        let result = self.extended(sql, parameters, deadline, Keep::Count);
        self.settle(result)
    }

    /// Parse, Bind, Describe, Execute and Sync in one write, read to the
    /// ReadyForQuery: one round trip.
    fn extended(
        &mut self,
        sql: &str,
        parameters: &[&dyn Encode],
        deadline: Instant,
        mut keep: Keep<'_>,
    ) -> Result<u64, Error> {
        let count = i16::try_from(parameters.len()).map_err(|_| Error::Invalid)?;
        wire::parse(
            &mut self.out,
            sql,
            parameters.iter().map(|parameter| parameter.type_oid()),
        )?;
        let at = wire::begin(&mut self.out, b'B')?;
        wire::put(&mut self.out, b"\0\0")?;
        // One format code, binary, for every parameter and every column.
        wire::put_i16(&mut self.out, 1)?;
        wire::put_i16(&mut self.out, 1)?;
        wire::put_i16(&mut self.out, count)?;
        for parameter in parameters {
            let length_at = self.out.len();
            wire::put_i32(&mut self.out, 0)?;
            let length = if parameter.encode(&mut self.out)? {
                let length = self.out.len().saturating_sub(length_at + 4);
                i32::try_from(length).map_err(|_| Error::Limit)?
            } else {
                -1
            };
            wire::put_at(&mut self.out, length_at, length)?;
        }
        wire::put_i16(&mut self.out, 1)?;
        wire::put_i16(&mut self.out, 1)?;
        wire::finish(&mut self.out, at)?;
        wire::describe_execute_sync(&mut self.out)?;
        self.send(deadline)?;

        #[derive(Clone, Copy, PartialEq)]
        enum Phase {
            Parse,
            Bind,
            Describe,
            Rows,
            Complete,
        }
        let mut phase = Phase::Parse;
        let mut width: Option<usize> = None;
        let mut rows = 0usize;
        let mut affected_rows = 0u64;
        let mut failed = None;
        let mut budget = self.limits.messages;
        loop {
            let (kind, start, end) = self.next(deadline, &mut budget)?;
            let message = wire::backend(kind, self.inbox.body(start, end))?;
            match (phase, message) {
                (_, Backend::ErrorResponse(error)) if failed.is_none() => {
                    if error.severity() != Severity::Error {
                        return Err(Error::Db(error));
                    }
                    failed = Some(error);
                }
                (_, Backend::ReadyForQuery(status))
                    if failed.is_some() || phase == Phase::Complete =>
                {
                    self.status = status;
                    self.pending = false;
                    return match failed {
                        Some(error) => Err(Error::Db(error)),
                        None => Ok(affected_rows),
                    };
                }
                _ if failed.is_some() => return Err(Error::Protocol),
                (Phase::Parse, Backend::ParseComplete) => phase = Phase::Bind,
                (Phase::Bind, Backend::BindComplete) => phase = Phase::Describe,
                (Phase::Describe, Backend::RowDescription(description)) => {
                    if description.len() > self.limits.columns {
                        return Err(Error::Limit);
                    }
                    if let Keep::Rows(kept) = &mut keep {
                        kept.describe(&description, &self.limits)?;
                    } else if description.fields().any(|field| field.format_code != 1) {
                        return Err(Error::Protocol);
                    }
                    width = Some(description.len());
                    phase = Phase::Rows;
                }
                (Phase::Describe, Backend::NoData) => phase = Phase::Rows,
                (Phase::Rows, Backend::DataRow(row)) if width == Some(row.len()) => {
                    rows = rows.checked_add(1).ok_or(Error::Limit)?;
                    if rows > self.limits.rows {
                        return Err(Error::Limit);
                    }
                    if let Keep::Rows(kept) = &mut keep {
                        kept.keep(&row, &self.limits)?;
                    }
                }
                (Phase::Rows, Backend::CommandComplete(tag)) => {
                    affected_rows = affected(tag)?;
                    phase = Phase::Complete;
                }
                (Phase::Rows, Backend::EmptyQueryResponse) if width.is_none() => {
                    phase = Phase::Complete;
                }
                _ => return Err(Error::Protocol),
            }
        }
    }

    /// Wait until a notification is held or `until` passes, reading only
    /// asynchronous messages. With `until` already past, this reads what
    /// is ready and does not wait, which also finds a connection the
    /// server has ended. A partial message left at `until` stays buffered.
    pub fn wait(&mut self, until: Instant) -> Result<(), Error> {
        if self.broken {
            return Err(Error::Broken);
        }
        let mut budget = self.limits.messages;
        let result = loop {
            if !self.notifications.is_empty() {
                break Ok(());
            }
            let (tag, start, end) = match self.receive(until, &mut budget, true) {
                Ok(Some(found)) => found,
                Ok(None) => break Ok(()),
                Err(error) => break Err(error),
            };
            match self.asynchronous(tag, start, end) {
                Ok(true) => {}
                Ok(false) => {
                    // An idle session's server may still end it, FATAL.
                    break match wire::backend(tag, self.inbox.body(start, end)) {
                        Ok(Backend::ErrorResponse(error)) => Err(Error::Db(error)),
                        Ok(_) => Err(Error::Protocol),
                        Err(error) => Err(error),
                    };
                }
                Err(error) => break Err(error),
            }
        };
        if result.is_err() {
            self.broken = true;
        }
        result
    }

    /// The oldest notification held, if any.
    pub fn take_notification(&mut self) -> Option<Notification> {
        self.notifications.pop_front()
    }

    /// How many notifications arrived past `Limits::notifications`, or
    /// could not be copied, and were dropped.
    pub fn dropped_notifications(&self) -> u64 {
        self.dropped
    }

    /// Send Terminate and close.
    pub fn close(mut self, deadline: Instant) -> Result<(), Error> {
        let result = self.terminate(deadline);
        self.broken = true;
        result
    }

    fn terminate(&mut self, deadline: Instant) -> Result<(), Error> {
        if self.broken {
            return Err(Error::Broken);
        }
        self.broken = true;
        self.stream
            .write_all(&wire::TERMINATE, deadline)
            .map_err(stream_error)
    }
}

impl<S: Stream> Drop for Client<S> {
    fn drop(&mut self) {
        let _ = self.terminate(Instant::now() + TERMINATE_WAIT);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::wire::tests::{frame, Script};
    use crate::{oid, Array, DbError, SqlState};

    const NONCE: &[u8] = b"rOprNGfwEbeRWgbNEkqO";
    const SERVER_FIRST: &[u8] =
        b"r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096";
    const SERVER_FINAL: &[u8] = b"v=3HO6Qt1M4MKJrmlKaoOqLAI0/0TV0HZe7J9H3MBtSOg=";
    const PROOF: &[u8] = b"p=qvT2SWdEH5Q06albL+hjSYuUhCG7VndFyzIb7CK4n9k=";

    fn limits() -> Limits {
        Limits {
            message_bytes: 64 * 1024,
            request_bytes: 64 * 1024,
            messages: 64,
            rows: 4,
            columns: 4,
            result_bytes: 4096,
            notifications: 2,
            scram_iterations: 4096,
        }
    }

    fn config() -> Config<'static> {
        Config {
            user: "td",
            database: "mail",
            password: b"pencil",
            application_name: "td-postgres-test",
            parameters: &[("statement_timeout", "5000")],
        }
    }

    fn auth(method: u32, data: &[u8]) -> Vec<u8> {
        let mut body = method.to_be_bytes().to_vec();
        body.extend_from_slice(data);
        frame(b'R', &body)
    }

    fn parameter(name: &str, value: &str) -> Vec<u8> {
        frame(b'S', format!("{name}\0{value}\0").as_bytes())
    }

    /// A server's whole startup: SCRAM, then its parameters and key.
    fn handshake() -> Vec<u8> {
        [
            auth(10, b"SCRAM-SHA-256-PLUS\0SCRAM-SHA-256\0\0"),
            auth(11, SERVER_FIRST),
            auth(12, SERVER_FINAL),
            auth(0, b""),
            parameter("client_encoding", "UTF8"),
            parameter("server_version", "17.10"),
            frame(b'K', &[0, 0, 0, 1, 0, 0, 0, 2]),
            frame(b'Z', b"I"),
        ]
        .concat()
    }

    fn connect(server: Vec<u8>) -> Result<Client<Script>, Error> {
        connect_with(server, limits())
    }

    fn connect_with(server: Vec<u8>, limits: Limits) -> Result<Client<Script>, Error> {
        let mut exchange = Exchange::with_nonce(b"", b"pencil", NONCE)?;
        let stream = Script::new(server, 7);
        Client::start(stream, &config(), limits, later(), &mut exchange)
    }

    fn later() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    /// A connected client whose server will send `then`.
    fn session(then: &[Vec<u8>]) -> Client<Script> {
        let mut client = connect(handshake()).unwrap();
        client.stream.input.extend(then.concat());
        client.stream.written.clear();
        client
    }

    fn complete(tag: &str) -> Vec<u8> {
        frame(b'C', format!("{tag}\0").as_bytes())
    }

    fn ready(status: u8) -> Vec<u8> {
        frame(b'Z', &[status])
    }

    fn error(severity: &str, code: &str) -> Vec<u8> {
        frame(
            b'E',
            format!("S{severity}\0V{severity}\0C{code}\0Msecret\0\0").as_bytes(),
        )
    }

    /// RowDescription of binary columns with these names and types.
    fn describe(columns: &[(&str, u32)]) -> Vec<u8> {
        let mut body = (columns.len() as i16).to_be_bytes().to_vec();
        for (name, oid) in columns {
            body.extend_from_slice(name.as_bytes());
            body.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0]);
            body.extend_from_slice(&oid.to_be_bytes());
            body.extend_from_slice(&[255, 255, 255, 255, 255, 255, 0, 1]);
        }
        frame(b'T', &body)
    }

    fn data(values: &[Option<&[u8]>]) -> Vec<u8> {
        let mut body = (values.len() as i16).to_be_bytes().to_vec();
        for value in values {
            match value {
                Some(bytes) => {
                    body.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
                    body.extend_from_slice(bytes);
                }
                None => body.extend_from_slice(&(-1i32).to_be_bytes()),
            }
        }
        frame(b'D', &body)
    }

    fn select(rows: &[&[Option<&[u8]>]]) -> Vec<Vec<u8>> {
        let mut out = vec![
            frame(b'1', b""),
            frame(b'2', b""),
            describe(&[("id", oid::INT8), ("body", oid::BYTEA)]),
        ];
        for row in rows {
            out.push(data(row));
        }
        out.push(complete(&format!("SELECT {}", rows.len())));
        out.push(ready(b'I'));
        out
    }

    #[test]
    fn startup_authenticates_with_scram_and_sends_what_was_configured() {
        let client = connect(handshake()).unwrap();
        let written = &client.stream.written;
        let startup: &[u8] = b"user\0td\0database\0mail\0application_name\0td-postgres-test\0\
            client_encoding\0UTF8\0statement_timeout\x005000\0\0";
        assert_eq!(&written[8..8 + startup.len()], startup);
        let rest = &written[8 + startup.len()..];
        let initial: &[u8] = b"p\0\0\0\x32SCRAM-SHA-256\0\0\0\0\x1cn,,n=,r=rOprNGfwEbeRWgbNEkqO";
        assert_eq!(&rest[..initial.len()], initial);
        assert!(rest.ends_with(PROOF));
        assert_eq!(client.transaction_status(), TransactionStatus::Idle);
        assert!(!client.is_broken());
    }

    #[test]
    fn the_server_must_complete_scram_before_it_is_trusted() {
        let without = |skip: &[usize]| -> Vec<u8> {
            let parts = [
                auth(10, b"SCRAM-SHA-256\0\0"),
                auth(11, SERVER_FIRST),
                auth(12, SERVER_FINAL),
                auth(0, b""),
                parameter("client_encoding", "UTF8"),
                frame(b'Z', b"I"),
            ];
            parts
                .iter()
                .enumerate()
                .filter(|(at, _)| !skip.contains(at))
                .flat_map(|(_, part)| part.clone())
                .collect()
        };
        assert!(connect(without(&[])).is_ok());
        // Trust: AuthenticationOk at once.
        assert_eq!(
            connect(without(&[0, 1, 2])).err(),
            Some(Error::Authentication)
        );
        // AuthenticationOk without the server's proof.
        assert_eq!(connect(without(&[2])).err(), Some(Error::Authentication));
        // A proof with nothing proved before it.
        assert_eq!(connect(without(&[1])).err(), Some(Error::Authentication));
        // Parameters and ReadyForQuery before AuthenticationOk.
        assert_eq!(connect(without(&[3])).err(), Some(Error::Protocol));
        // No client_encoding report.
        assert_eq!(connect(without(&[4])).err(), Some(Error::Protocol));
        for refused in [
            auth(3, b""),
            auth(5, b"salt"),
            auth(7, b""),
            auth(10, b"SCRAM-SHA-256-PLUS\0\0"),
        ] {
            assert_eq!(connect(refused).err(), Some(Error::Authentication));
        }
        let mut forged = without(&[]);
        let at = forged
            .windows(SERVER_FINAL.len())
            .position(|window| window == SERVER_FINAL)
            .unwrap();
        forged[at + 10] ^= 1;
        assert_eq!(connect(forged).err(), Some(Error::Authentication));
        let mut latin = handshake();
        let at = latin.windows(4).position(|w| w == b"UTF8").unwrap();
        latin.splice(at..at + 4, b"LAT1".iter().copied());
        assert_eq!(connect(latin).err(), Some(Error::Protocol));
        let mut busy = handshake();
        busy.truncate(busy.len() - 6);
        busy.extend(ready(b'T'));
        assert_eq!(connect(busy).err(), Some(Error::Protocol));
        let refused = [
            auth(10, b"SCRAM-SHA-256\0\0"),
            auth(11, SERVER_FIRST),
            error("FATAL", "28P01"),
        ]
        .concat();
        assert!(matches!(connect(refused), Err(Error::Db(e)) if e.code().as_str() == "28P01"));
        let mut few = limits();
        few.scram_iterations = 4095;
        assert_eq!(connect_with(handshake(), few).err(), Some(Error::Limit));
        assert_eq!(
            connect(handshake()[..40].to_vec()).err(),
            Some(Error::Closed)
        );
        few = limits();
        few.messages = 7;
        assert_eq!(connect_with(handshake(), few).err(), Some(Error::Limit));
    }

    #[test]
    fn configuration_is_refused_before_connecting() {
        let mut bad = config();
        bad.password = "pénc".as_bytes();
        assert_eq!(check(&bad, &limits()), Err(Error::Invalid));
        for parameters in [
            &[("user", "x")][..],
            &[("replication", "database")],
            &[("_pq_.x", "1")],
            &[("a", "1"), ("a", "2")],
            &[("", "1")],
            &[("a", "1\0")],
        ] {
            let mut bad = config();
            bad.parameters = parameters;
            assert_eq!(
                check(&bad, &limits()),
                Err(Error::Invalid),
                "{parameters:?}"
            );
        }
        let mut few = limits();
        few.message_bytes = 255;
        assert_eq!(check(&config(), &few), Err(Error::Invalid));
        few = limits();
        few.scram_iterations = 0;
        assert_eq!(check(&config(), &few), Err(Error::Invalid));
    }

    #[test]
    fn a_query_sends_typed_binary_parameters_and_reads_binary_rows() {
        let mut client = session(&select(&[
            &[Some(&7i64.to_be_bytes()), Some(b"ab")],
            &[Some(&8i64.to_be_bytes()), None],
        ]));
        let rows = client
            .query(
                "SELECT id, body FROM t WHERE id > $1 AND body = $2",
                &[&5i64, &None::<&[u8]>],
                later(),
            )
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows.column_name(1), Some("body"));
        let first = rows.row(0).unwrap();
        assert_eq!(first.get::<i64>(0), Ok(7));
        assert_eq!(first.get_named::<&[u8]>("body"), Ok(&b"ab"[..]));
        assert_eq!(rows.row(1).unwrap().get::<Option<&[u8]>>(1), Ok(None));
        assert_eq!(first.get::<i32>(0), Err(crate::DecodeError::Type(20)));
        assert_eq!(first.get::<i64>(2), Err(crate::DecodeError::Column));
        let written = &client.stream.written;
        assert_eq!(written[0], b'P');
        // Parse names both parameter types: int8, then bytea.
        assert!(written
            .windows(10)
            .any(|w| w == [0, 2, 0, 0, 0, 20, 0, 0, 0, 17]));
        assert!(written.ends_with(b"S\0\0\0\x04"));
    }

    #[test]
    fn an_error_leaves_the_session_and_a_fatal_one_ends_it() {
        let mut client = session(&[frame(b'1', b""), error("ERROR", "23505"), ready(b'I')]);
        let got = client.execute("INSERT", &[], later());
        assert!(matches!(got, Err(Error::Db(e))
            if e.code().as_str() == "23505" && e.severity() == Severity::Error));
        assert!(!client.is_broken());
        client.stream.input.extend(select(&[]).concat());
        assert_eq!(
            client.query("SELECT", &[], later()).map(|rows| rows.len()),
            Ok(0)
        );
        client.stream.input.extend(error("FATAL", "57P01"));
        assert!(matches!(
            client.execute("SELECT", &[], later()),
            Err(Error::Db(_))
        ));
        assert!(client.is_broken());
        assert_eq!(client.execute("SELECT", &[], later()), Err(Error::Broken));
    }

    #[test]
    fn a_hostile_answer_breaks_the_connection() {
        let row = data(&[Some(&1i64.to_be_bytes()), None]);
        let start = || vec![frame(b'1', b""), frame(b'2', b"")];
        let with = |mut first: Vec<Vec<u8>>, rest: Vec<Vec<u8>>| {
            first.extend(rest);
            first
        };
        let big: &[u8] = &[0; 3000];
        let cases: Vec<(Vec<Vec<u8>>, Error)> = vec![
            // Rows before their description, or of the wrong width.
            (with(start(), vec![row.clone()]), Error::Protocol),
            (
                with(start(), vec![describe(&[("id", oid::INT8)]), row.clone()]),
                Error::Protocol,
            ),
            // ReadyForQuery before the command completes; steps skipped.
            (
                with(start(), vec![frame(b'n', b""), ready(b'I')]),
                Error::Protocol,
            ),
            (vec![frame(b'2', b"")], Error::Protocol),
            (with(start(), vec![frame(b's', b"")]), Error::Protocol),
            (with(start(), vec![frame(b'G', b"\0\0\0")]), Error::Protocol),
            // More rows than allowed; a count that is not one.
            (select(&[&[None::<&[u8]>, None][..]; 5]), Error::Limit),
            (
                with(
                    start(),
                    vec![frame(b'n', b""), complete("UPDATE x"), ready(b'I')],
                ),
                Error::Protocol,
            ),
            // Five columns, past four.
            (
                with(
                    start(),
                    vec![describe(&[
                        ("a", 20),
                        ("b", 20),
                        ("c", 20),
                        ("d", 20),
                        ("e", 20),
                    ])],
                ),
                Error::Limit,
            ),
            // A result past its byte allowance.
            (
                select(&[&[None, Some(big)], &[None, Some(big)]]),
                Error::Limit,
            ),
            // Two errors, and anything but ReadyForQuery after one.
            (
                vec![error("ERROR", "42601"), error("ERROR", "42601")],
                Error::Protocol,
            ),
            (
                vec![error("ERROR", "42601"), frame(b'1', b"")],
                Error::Protocol,
            ),
            // A connection closed mid-answer.
            (vec![frame(b'1', b"")], Error::Closed),
        ];
        for (answer, expected) in cases {
            let mut client = session(&answer);
            assert_eq!(
                client.query("SELECT", &[], later()).err(),
                Some(expected),
                "{answer:?}"
            );
            assert!(client.is_broken());
        }
        let mut client = session(&[frame(b'1', b"")]);
        client.stream.stall = true;
        assert_eq!(
            client.execute("SELECT pg_sleep(9)", &[], later()),
            Err(Error::Deadline)
        );
        assert!(client.is_broken());
        // A flood of notices is bounded by the message allowance.
        let notice = frame(b'N', b"SNOTICE\0Mx\0\0");
        let mut client = session(&vec![notice; 100]);
        assert_eq!(client.execute("SELECT", &[], later()), Err(Error::Limit));
        // The client encoding may not move off UTF8.
        let mut client = session(&[parameter("client_encoding", "SQL_ASCII")]);
        assert_eq!(client.execute("SET", &[], later()), Err(Error::Protocol));
    }

    #[test]
    fn refusals_before_sending_leave_the_connection_usable() {
        let mut client = session(&[]);
        let big = vec![0u8; 70 * 1024];
        assert_eq!(
            client.execute("SELECT $1", &[&big.as_slice()], later()),
            Err(Error::Limit)
        );
        assert_eq!(
            client.execute("SELECT '\0'", &[], later()),
            Err(Error::Invalid)
        );
        assert_eq!(
            client.execute("SELECT", &[], Instant::now()),
            Err(Error::Deadline)
        );
        assert!(!client.is_broken());
        assert!(client.stream.written.is_empty());
        client
            .stream
            .input
            .extend(select(&[&[None, None], &[None, None]]).concat());
        assert_eq!(
            client.query_one("SELECT", &[], later()).err(),
            Some(Error::RowCount)
        );
        assert!(!client.is_broken());
        client.stream.input.extend(select(&[]).concat());
        assert_eq!(
            client.query_one("SELECT", &[], later()).err(),
            Some(Error::RowCount)
        );
        client.stream.input.extend(select(&[]).concat());
        assert!(client.query_opt("SELECT", &[], later()).unwrap().is_none());
    }

    #[test]
    fn execute_reports_the_rows_its_tag_counts() {
        assert_eq!(affected(b"INSERT 0 3"), Ok(3));
        assert_eq!(affected(b"UPDATE 18446744073709551615"), Ok(u64::MAX));
        assert_eq!(affected(b"CREATE TABLE"), Ok(0));
        assert_eq!(affected(b"BEGIN"), Ok(0));
        for bad in [
            &b"INSERT 3"[..],
            b"DELETE",
            b"DELETE -1",
            b"DELETE 1 2",
            b"SELECT +1",
            b"UPDATE 18446744073709551616",
        ] {
            assert_eq!(affected(bad), Err(Error::Protocol));
        }
        let mut client = session(&[
            frame(b'1', b""),
            frame(b'2', b""),
            frame(b'n', b""),
            complete("DELETE 2"),
            ready(b'T'),
        ]);
        assert_eq!(client.execute("DELETE", &[], later()), Ok(2));
        assert_eq!(
            client.transaction_status(),
            TransactionStatus::InTransaction
        );
    }

    fn in_transaction(then: &[Vec<u8>]) -> Client<Script> {
        let mut client = session(&[complete("BEGIN"), ready(b'T')]);
        client.batch("BEGIN", later()).unwrap();
        client.stream.input.extend(then.concat());
        client
    }

    #[test]
    fn commit_tells_committed_refused_and_unknown_apart() {
        let mut client = in_transaction(&[complete("COMMIT"), ready(b'I')]);
        assert_eq!(client.commit(later()), Ok(()));
        assert_eq!(
            client.commit(later()),
            Err(CommitError::NotSent(Error::Invalid))
        );

        let mut client = in_transaction(&[complete("ROLLBACK"), ready(b'I')]);
        assert_eq!(client.commit(later()), Err(CommitError::RolledBack(None)));
        assert!(!client.is_broken());

        let mut client = in_transaction(&[error("ERROR", "40001"), ready(b'I')]);
        let serialization = DbError::new(Severity::Error, SqlState::new(b"40001").unwrap());
        assert_eq!(
            client.commit(later()),
            Err(CommitError::RolledBack(Some(serialization)))
        );
        assert!(!client.is_broken());

        // An answer that ends before ReadyForQuery is unknown, however far
        // it got: the COMMIT tag alone is not taken as proof.
        for answer in [
            vec![],
            vec![complete("COMMIT")],
            vec![frame(b'N', b"SWARNING\0Mx\0\0")],
            vec![error("FATAL", "57P01")],
            vec![error("ERROR", "40001")],
            vec![complete("COMMIT"), ready(b'T')],
            vec![complete("SELECT 1"), ready(b'I')],
        ] {
            let mut client = in_transaction(&answer);
            assert!(
                matches!(client.commit(later()), Err(CommitError::OutcomeUnknown(_))),
                "{answer:?}"
            );
            assert!(client.is_broken());
            assert_eq!(
                client.commit(later()),
                Err(CommitError::NotSent(Error::Broken))
            );
        }
        let mut client = in_transaction(&[]);
        client.stream.stall = true;
        assert_eq!(
            client.commit(later()),
            Err(CommitError::OutcomeUnknown(Error::Deadline))
        );
        let mut client = in_transaction(&[]);
        assert_eq!(
            client.commit(Instant::now()),
            Err(CommitError::NotSent(Error::Deadline))
        );
        assert!(!client.is_broken());
    }

    #[test]
    fn batches_and_rollback_read_every_statement() {
        let mut client = session(&[
            complete("BEGIN"),
            complete("SET"),
            describe(&[("pg_advisory_xact_lock", 2278)]),
            data(&[Some(b"")]),
            complete("SELECT 1"),
            ready(b'T'),
            complete("ROLLBACK"),
            ready(b'I'),
            error("ERROR", "42601"),
            ready(b'I'),
            frame(b'I', b""),
            ready(b'I'),
        ]);
        client
            .batch(
                "BEGIN; SET LOCAL x=1; SELECT pg_advisory_xact_lock(1)",
                later(),
            )
            .unwrap();
        assert_eq!(
            client.transaction_status(),
            TransactionStatus::InTransaction
        );
        client.rollback(later()).unwrap();
        assert!(matches!(client.batch("SELEC", later()), Err(Error::Db(_))));
        client.batch("", later()).unwrap();
        assert!(!client.is_broken());
        let mut client = session(&[complete("COMMIT"), ready(b'I')]);
        assert_eq!(client.rollback(later()), Err(Error::Protocol));
        assert!(client.is_broken());
        let mut client = session(&[frame(b'H', b"\0\0\0")]);
        assert_eq!(
            client.batch("COPY t TO STDOUT", later()),
            Err(Error::Protocol)
        );
    }

    fn notify(channel: &str, payload: &str) -> Vec<u8> {
        let mut body = 42i32.to_be_bytes().to_vec();
        body.extend_from_slice(format!("{channel}\0{payload}\0").as_bytes());
        frame(b'A', &body)
    }

    #[test]
    fn notifications_are_held_bounded_and_waited_for() {
        let mut client = session(&[]);
        client.stream.stall = true;
        assert_eq!(client.wait(Instant::now()), Ok(()));
        assert!(client.take_notification().is_none());
        client
            .stream
            .input
            .extend([frame(b'N', b"SNOTICE\0Mx\0\0"), notify("td_mta_relay", "")].concat());
        assert_eq!(client.wait(later()), Ok(()));
        let note = client.take_notification().unwrap();
        assert_eq!(
            (note.channel(), note.payload(), note.process_id()),
            ("td_mta_relay", "", 42)
        );
        // Notifications during a query are held too, past two dropped.
        let mut answer = vec![notify("a", "1"), notify("b", "2"), notify("c", "3")];
        answer.extend(select(&[]));
        client.stream.input.extend(answer.concat());
        client.query("SELECT", &[], later()).unwrap();
        assert_eq!(client.take_notification().unwrap().channel(), "a");
        assert_eq!(client.take_notification().unwrap().channel(), "b");
        assert!(client.take_notification().is_none());
        assert_eq!(client.dropped_notifications(), 1);
        // A partial message left at the deadline is resumed later.
        let whole = notify("half", "");
        client.stream.input.extend(&whole[..5]);
        assert_eq!(
            client.wait(Instant::now() + Duration::from_millis(5)),
            Ok(())
        );
        assert!(client.take_notification().is_none());
        client.stream.input.extend(&whole[5..]);
        assert_eq!(client.wait(later()), Ok(()));
        assert_eq!(client.take_notification().unwrap().channel(), "half");
        // An idle session the server ends, or answers out of turn, breaks.
        client.stream.input.extend(error("FATAL", "57P05"));
        assert!(matches!(client.wait(later()), Err(Error::Db(_))));
        assert!(client.is_broken());
        let mut client = session(&[ready(b'I')]);
        assert_eq!(client.wait(later()), Err(Error::Protocol));
        let mut client = session(&[]);
        assert_eq!(client.wait(later()), Err(Error::Closed));
    }

    #[test]
    fn arrays_and_names_read_through_a_row() {
        let ids: Vec<&[u8]> = vec![b"x", b"yz"];
        let mut encoded = Vec::new();
        ids.encode(&mut encoded).unwrap();
        let mut client = session(&[
            frame(b'1', b""),
            frame(b'2', b""),
            describe(&[("ids", oid::BYTEA_ARRAY), ("who", oid::NAME)]),
            data(&[Some(&encoded), Some(b"td")]),
            complete("SELECT 1"),
            ready(b'I'),
        ]);
        let row = client.query_one("SELECT", &[&ids], later()).unwrap();
        let array: Array<'_> = row.get(0).unwrap();
        let read: Vec<&[u8]> = array.iter().collect::<Result<_, _>>().unwrap();
        assert_eq!(read, ids);
        assert_eq!(row.get::<&str>(1), Ok("td"));
        assert_eq!(row.column_name(1), Some("who"));
    }

    #[test]
    fn close_sends_terminate_and_leaves_nothing_usable() {
        let mut client = session(&[]);
        assert_eq!(client.terminate(later()), Ok(()));
        assert_eq!(client.stream.written, wire::TERMINATE);
        assert_eq!(client.execute("SELECT", &[], later()), Err(Error::Broken));
        assert_eq!(client.wait(later()), Err(Error::Broken));
    }

    #[test]
    fn a_commit_answer_is_believed_only_in_its_whole_context() {
        // The tag says the commit happened; an ERROR after it cannot undo
        // that, so nothing is known.
        let mut client =
            in_transaction(&[complete("COMMIT"), error("ERROR", "XX000"), ready(b'I')]);
        assert_eq!(
            client.commit(later()),
            Err(CommitError::OutcomeUnknown(Error::Protocol))
        );
        assert!(client.is_broken());
        // An ERROR that leaves the block failed: rolled back, and the
        // connection, still in that block, is not handed back.
        let mut client = in_transaction(&[error("ERROR", "57014"), ready(b'E')]);
        assert!(matches!(
            client.commit(later()),
            Err(CommitError::RolledBack(Some(_)))
        ));
        assert!(client.is_broken());
        // An ERROR that leaves the transaction live is no rollback.
        let mut client = in_transaction(&[error("ERROR", "57014"), ready(b'T')]);
        assert_eq!(
            client.commit(later()),
            Err(CommitError::OutcomeUnknown(Error::Protocol))
        );
        assert!(client.is_broken());
        // Nor is a second answer to the one statement, either way round.
        for answer in [
            [complete("COMMIT"), complete("ROLLBACK"), ready(b'I')],
            [complete("ROLLBACK"), complete("COMMIT"), ready(b'I')],
            [complete("SELECT 1"), complete("COMMIT"), ready(b'I')],
        ] {
            let mut client = in_transaction(&answer);
            assert_eq!(
                client.commit(later()),
                Err(CommitError::OutcomeUnknown(Error::Protocol))
            );
            assert!(client.is_broken());
        }
        // Nor is a ROLLBACK tag that leaves one open.
        let mut client = in_transaction(&[complete("ROLLBACK"), ready(b'E')]);
        assert_eq!(
            client.commit(later()),
            Err(CommitError::OutcomeUnknown(Error::Protocol))
        );
    }

    #[test]
    fn a_failed_write_reports_the_fatal_that_ended_the_session() {
        let mut client = session(&[error("FATAL", "25P03")]);
        client.stream.refuse_writes = true;
        let ended = client.execute("SELECT", &[], later());
        assert!(matches!(ended, Err(Error::Db(e)) if e.code().as_str() == "25P03"));
        assert!(client.is_broken());
        // Written or not, COMMIT's outcome stays unknown.
        let mut client =
            in_transaction(&[frame(b'N', b"SNOTICE\0Mx\0\0"), error("FATAL", "57P01")]);
        client.stream.refuse_writes = true;
        assert!(matches!(
            client.commit(later()),
            Err(CommitError::OutcomeUnknown(Error::Db(_)))
        ));
        // Nor past a message it cannot read.
        let mut client = session(&[frame(b'?', b""), error("FATAL", "57P01")]);
        client.stream.refuse_writes = true;
        assert_eq!(client.execute("SELECT", &[], later()), Err(Error::Io));
        // With nothing waiting, it is the transport's failure.
        let mut client = session(&[]);
        client.stream.refuse_writes = true;
        assert_eq!(client.execute("SELECT", &[], later()), Err(Error::Io));
        let mut client = session(&[error("ERROR", "42601"), ready(b'I')]);
        client.stream.refuse_writes = true;
        assert_eq!(client.execute("SELECT", &[], later()), Err(Error::Io));
        assert!(client.is_broken());
    }

    #[test]
    fn every_statement_must_be_answered() {
        let mut client = session(&[ready(b'I')]);
        assert_eq!(client.batch("SELECT 1", later()), Err(Error::Protocol));
        assert!(client.is_broken());
        let mut client = session(&[frame(b'I', b""), ready(b'I')]);
        assert_eq!(client.batch("", later()), Ok(()));
    }

    #[test]
    fn only_notices_may_come_before_authentication() {
        let early = |message: Vec<u8>| {
            [
                auth(10, b"SCRAM-SHA-256\0\0"),
                message,
                auth(11, SERVER_FIRST),
                auth(12, SERVER_FINAL),
                auth(0, b""),
                parameter("client_encoding", "UTF8"),
                ready(b'I'),
            ]
            .concat()
        };
        assert!(connect(early(frame(b'N', b"SNOTICE\0Mx\0\0"))).is_ok());
        assert_eq!(
            connect(early(parameter("client_encoding", "UTF8"))).err(),
            Some(Error::Protocol)
        );
        assert_eq!(
            connect(early(notify("td_mta_relay", ""))).err(),
            Some(Error::Protocol)
        );
    }

    #[test]
    fn a_notification_larger_than_postgresql_sends_is_dropped() {
        let long_channel = "c".repeat(64);
        let long_payload = "p".repeat(8000);
        let mut client = session(&[
            notify(&long_channel, ""),
            notify("relay", &long_payload),
            notify(&"c".repeat(63), &"p".repeat(7999)),
        ]);
        assert_eq!(client.wait(later()), Ok(()));
        assert_eq!(client.dropped_notifications(), 2);
        assert_eq!(client.take_notification().unwrap().payload().len(), 7999);
    }

    #[test]
    fn a_refused_allocation_breaks_only_what_was_written() {
        let answer = select(&[&[Some(&7i64.to_be_bytes()), Some(b"ab")]]);
        let parameter = vec![b'x'; 600];
        let mut refusals = 0;
        for allowed in 0.. {
            let mut client = session(&answer);
            // A fresh output buffer, so building the request reserves too.
            client.out = Vec::new();
            let got = td_alloc::with_reservation_limit(allowed, || {
                client.query("SELECT $1", &[&parameter], later())
            });
            match got {
                Ok(rows) => {
                    assert_eq!(rows.len(), 1);
                    break;
                }
                Err(error) => {
                    assert_eq!(error, Error::Allocation, "allowance {allowed}");
                    refusals += 1;
                }
            }
            // Refused before writing: usable and silent. After: broken.
            assert_eq!(client.is_broken(), !client.stream.written.is_empty());
            if !client.is_broken() {
                client.stream.input.extend(answer.concat());
                assert_eq!(client.query("SELECT", &[], later()).map(|r| r.len()), Ok(1));
            }
        }
        assert!(refusals >= 2, "{refusals}");

        // A notification whose copy is refused is dropped, and counted.
        let mut client = session(&[notify("td_mta_relay", "")]);
        client.stream.stall = true;
        let until = Instant::now() + Duration::from_millis(20);
        let waited = td_alloc::with_reservation_limit(0, || client.wait(until));
        assert_eq!(waited, Ok(()));
        assert!(client.take_notification().is_none());
        assert_eq!(client.dropped_notifications(), 1);
        assert!(!client.is_broken());
    }

    /// Counts what passes through it, and refuses reads past a budget.
    struct Counted {
        inner: Socket,
        seen: std::rc::Rc<std::cell::Cell<(usize, usize)>>,
        budget: usize,
    }

    impl Stream for Counted {
        fn read(&mut self, buffer: &mut [u8], deadline: Instant) -> Result<usize, StreamError> {
            let count = self.inner.read(buffer, deadline)?;
            let (read, written) = self.seen.get();
            self.seen.set((read + count, written));
            if read + count > self.budget {
                return Err(StreamError::Failed);
            }
            Ok(count)
        }

        fn write_all(&mut self, bytes: &[u8], deadline: Instant) -> Result<(), StreamError> {
            let (read, written) = self.seen.get();
            self.seen.set((read, written + bytes.len()));
            self.inner.write_all(bytes, deadline)
        }
    }

    /// A server on `path` that reads one startup message, answers
    /// `answer`, and returns the startup's length once the client goes.
    fn serve_once(path: &Path, answer: Vec<u8>) -> std::thread::JoinHandle<usize> {
        use std::io::{Read, Write};
        let listener = std::os::unix::net::UnixListener::bind(path).unwrap();
        std::thread::spawn(move || {
            let (mut peer, _) = listener.accept().unwrap();
            let mut length = [0u8; 4];
            peer.read_exact(&mut length).unwrap();
            let mut rest = vec![0u8; u32::from_be_bytes(length) as usize - 4];
            peer.read_exact(&mut rest).unwrap();
            peer.write_all(&answer).unwrap();
            let mut tail = Vec::new();
            let _ = peer.read_to_end(&mut tail);
            4 + rest.len()
        })
    }

    #[test]
    fn a_wrapped_unix_session_runs_its_startup_through_the_wrapper() {
        let root = td_fs::TempDir::new("td-postgres-wrap").unwrap();
        for (budget, answer, expected) in [
            // The server asks for MD5: the wrapper carried the whole
            // startup out and the whole answer in.
            (usize::MAX, auth(5, b"salt"), Error::Authentication),
            // A wrapper that refuses the server's first bytes ends the
            // login there, before the client takes them for a request.
            (4, auth(10, b"SCRAM-SHA-256\0\0"), Error::Io),
        ] {
            let path = root.path().join(format!("socket-{budget}"));
            let length = answer.len();
            let server = serve_once(&path, answer);
            let seen = std::rc::Rc::new(std::cell::Cell::new((0, 0)));
            let result = connect_unix_through(
                &path,
                |inner| Counted {
                    inner,
                    seen: seen.clone(),
                    budget,
                },
                &config(),
                limits(),
                later(),
            );
            assert_eq!(result.err(), Some(expected));
            let (read, written) = seen.get();
            // Checked before joining: a failed connect would leave the
            // server waiting in accept.
            assert_eq!(read, length);
            let startup = server.join().unwrap();
            assert_eq!(written, startup);
        }
    }
}
