//! Protocol 3.0 framing: the messages this client writes, and a checked
//! reading of every message a server may send it. Nothing here trusts a
//! length, count or terminator the server sent: each is checked against
//! the bytes actually present, and a message must be consumed exactly.
use crate::stream::{Stream, StreamError};
use crate::{DbError, Error, Severity, SqlState, TransactionStatus};
use std::time::Instant;

/// PostgreSQL's own bound on one message, a gibibyte.
pub(crate) const MAX_MESSAGE_BYTES: usize = 1 << 30;
/// PostgreSQL's `MAX_STARTUP_PACKET_LENGTH`.
const MAX_STARTUP_BYTES: usize = 10_000;
const PROTOCOL_3_0: i32 = 196_608;
/// The read buffer's first size, grown to a message's size as needed.
const FIRST_BUFFER: usize = 8 * 1024;

// ---- What the client writes -------------------------------------------

/// Start a tagged message: its tag and a length patched by `finish`.
pub(crate) fn begin(out: &mut Vec<u8>, tag: u8) -> Result<usize, Error> {
    td_alloc::push(out, tag)?;
    let at = out.len();
    td_alloc::extend_from_slice(out, &[0; 4])?;
    Ok(at)
}

/// Patch the length of the message whose length field starts at `at`.
pub(crate) fn finish(out: &mut [u8], at: usize) -> Result<(), Error> {
    let length = out.len().checked_sub(at).ok_or(Error::Invalid)?;
    let length = i32::try_from(length).map_err(|_| Error::Limit)?;
    put_at(out, at, length)
}

/// Overwrite four bytes at `at` with `value`.
pub(crate) fn put_at(out: &mut [u8], at: usize, value: i32) -> Result<(), Error> {
    out.get_mut(at..at.checked_add(4).ok_or(Error::Invalid)?)
        .ok_or(Error::Invalid)?
        .copy_from_slice(&value.to_be_bytes());
    Ok(())
}

pub(crate) fn put(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), Error> {
    Ok(td_alloc::extend_from_slice(out, bytes)?)
}

pub(crate) fn put_i16(out: &mut Vec<u8>, value: i16) -> Result<(), Error> {
    put(out, &value.to_be_bytes())
}

pub(crate) fn put_i32(out: &mut Vec<u8>, value: i32) -> Result<(), Error> {
    put(out, &value.to_be_bytes())
}

pub(crate) fn put_u32(out: &mut Vec<u8>, value: u32) -> Result<(), Error> {
    put(out, &value.to_be_bytes())
}

/// A NUL-terminated string, which therefore holds no NUL itself.
pub(crate) fn put_cstr(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), Error> {
    if bytes.contains(&0) {
        return Err(Error::Invalid);
    }
    put(out, bytes)?;
    Ok(td_alloc::push(out, 0)?)
}

/// The untagged StartupMessage for protocol 3.0 with these parameters.
pub(crate) fn startup(out: &mut Vec<u8>, parameters: &[(&str, &str)]) -> Result<(), Error> {
    let at = out.len();
    put(out, &[0; 4])?;
    put_i32(out, PROTOCOL_3_0)?;
    for (name, value) in parameters {
        put_cstr(out, name.as_bytes())?;
        put_cstr(out, value.as_bytes())?;
    }
    td_alloc::push(out, 0)?;
    if out.len().saturating_sub(at) > MAX_STARTUP_BYTES {
        return Err(Error::Invalid);
    }
    finish(out, at)
}

/// SASLInitialResponse: the mechanism and the client-first message.
pub(crate) fn sasl_initial(out: &mut Vec<u8>, mechanism: &[u8], data: &[u8]) -> Result<(), Error> {
    let at = begin(out, b'p')?;
    put_cstr(out, mechanism)?;
    put_i32(out, i32::try_from(data.len()).map_err(|_| Error::Invalid)?)?;
    put(out, data)?;
    finish(out, at)
}

/// SASLResponse: the client-final message.
pub(crate) fn sasl_response(out: &mut Vec<u8>, data: &[u8]) -> Result<(), Error> {
    let at = begin(out, b'p')?;
    put(out, data)?;
    finish(out, at)
}

/// A simple Query.
pub(crate) fn query(out: &mut Vec<u8>, sql: &str) -> Result<(), Error> {
    let at = begin(out, b'Q')?;
    put_cstr(out, sql.as_bytes())?;
    finish(out, at)
}

/// Parse into the unnamed statement, with each parameter's type named.
pub(crate) fn parse(
    out: &mut Vec<u8>,
    sql: &str,
    types: impl ExactSizeIterator<Item = u32>,
) -> Result<(), Error> {
    let at = begin(out, b'P')?;
    put_cstr(out, b"")?;
    put_cstr(out, sql.as_bytes())?;
    put_i16(out, i16::try_from(types.len()).map_err(|_| Error::Invalid)?)?;
    for oid in types {
        put_u32(out, oid)?;
    }
    finish(out, at)
}

/// Describe the unnamed portal, Execute it for every row, then Sync.
pub(crate) fn describe_execute_sync(out: &mut Vec<u8>) -> Result<(), Error> {
    let at = begin(out, b'D')?;
    put(out, b"P\0")?;
    finish(out, at)?;
    let at = begin(out, b'E')?;
    put(out, b"\0")?;
    put_i32(out, 0)?;
    finish(out, at)?;
    let at = begin(out, b'S')?;
    finish(out, at)
}

/// Terminate.
pub(crate) const TERMINATE: [u8; 5] = [b'X', 0, 0, 0, 4];

// ---- What the server sends ----------------------------------------------

/// One backend message, borrowed from the read buffer.
#[derive(Debug)]
pub(crate) enum Backend<'a> {
    Authentication(Authentication<'a>),
    KeyData,
    BindComplete,
    CommandComplete(&'a [u8]),
    DataRow(DataRow<'a>),
    EmptyQueryResponse,
    ErrorResponse(DbError),
    NoData,
    NoticeResponse,
    Notification(Notice<'a>),
    ParameterStatus {
        name: &'a [u8],
        value: &'a [u8],
    },
    ParseComplete,
    ReadyForQuery(TransactionStatus),
    RowDescription(RowDescription<'a>),
    /// A message valid in protocol 3.0 that this client never asks for:
    /// COPY, function calls, a described statement's parameters, a
    /// suspended portal, CloseComplete, or a protocol negotiation.
    Unrequested,
}

#[derive(Debug)]
pub(crate) enum Authentication<'a> {
    Ok,
    Sasl(Mechanisms<'a>),
    SaslContinue(&'a [u8]),
    SaslFinal(&'a [u8]),
    /// Any other request: cleartext, MD5, Kerberos, GSSAPI, SSPI.
    Other,
}

/// A NotificationResponse's channel and payload, UTF-8.
#[derive(Debug)]
pub(crate) struct Notice<'a> {
    pub(crate) process_id: i32,
    pub(crate) channel: &'a str,
    pub(crate) payload: &'a str,
}

/// An AuthenticationSASL's mechanism names, checked well formed.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Mechanisms<'a>(&'a [u8]);

impl Mechanisms<'_> {
    pub(crate) fn offers(&self, mechanism: &[u8]) -> bool {
        let mut reader = Reader(self.0);
        while let Ok(name) = reader.cstr() {
            if name.is_empty() {
                return false;
            }
            if name == mechanism {
                return true;
            }
        }
        false
    }
}

/// A RowDescription whose fields have all been checked.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RowDescription<'a> {
    count: usize,
    fields: &'a [u8],
}

/// One described column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Field<'a> {
    pub(crate) name: &'a [u8],
    pub(crate) type_oid: u32,
    /// 0 for text, 1 for binary.
    pub(crate) format_code: i16,
}

impl<'a> RowDescription<'a> {
    fn parse(body: &'a [u8]) -> Result<Self, Error> {
        let mut reader = Reader(body);
        let count = count(reader.i16()?)?;
        let fields = reader.0;
        for _ in 0..count {
            field(&mut reader)?;
        }
        reader.end()?;
        Ok(Self { count, fields })
    }

    pub(crate) fn len(&self) -> usize {
        self.count
    }

    pub(crate) fn fields(&self) -> impl Iterator<Item = Field<'a>> {
        let mut reader = Reader(self.fields);
        (0..self.count).map_while(move |_| field(&mut reader).ok())
    }
}

fn field<'a>(reader: &mut Reader<'a>) -> Result<Field<'a>, Error> {
    let name = reader.cstr()?;
    let _table = reader.u32()?;
    let _attribute = reader.i16()?;
    let type_oid = reader.u32()?;
    let _size = reader.i16()?;
    let _modifier = reader.i32()?;
    let format_code = reader.i16()?;
    if format_code != 0 && format_code != 1 {
        return Err(Error::Protocol);
    }
    Ok(Field {
        name,
        type_oid,
        format_code,
    })
}

/// A DataRow whose values have all been checked to fit the message.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DataRow<'a> {
    count: usize,
    values: &'a [u8],
}

impl<'a> DataRow<'a> {
    fn parse(body: &'a [u8]) -> Result<Self, Error> {
        let mut reader = Reader(body);
        let count = count(reader.i16()?)?;
        let values = reader.0;
        for _ in 0..count {
            value(&mut reader)?;
        }
        reader.end()?;
        Ok(Self { count, values })
    }

    pub(crate) fn len(&self) -> usize {
        self.count
    }

    /// Each value's bytes, `None` for NULL.
    pub(crate) fn values(&self) -> impl Iterator<Item = Option<&'a [u8]>> {
        let mut reader = Reader(self.values);
        (0..self.count).map_while(move |_| value(&mut reader).ok())
    }
}

/// One length-prefixed value: -1 is NULL, any other negative is refused.
pub(crate) fn value<'a>(reader: &mut Reader<'a>) -> Result<Option<&'a [u8]>, Error> {
    match reader.i32()? {
        -1 => Ok(None),
        length => {
            let length = usize::try_from(length).map_err(|_| Error::Protocol)?;
            reader.take(length).map(Some)
        }
    }
}

/// A count sent as Int16, which may not be negative.
fn count(value: i16) -> Result<usize, Error> {
    usize::try_from(value).map_err(|_| Error::Protocol)
}

/// Read one backend message from its tag and body.
pub(crate) fn backend(tag: u8, body: &[u8]) -> Result<Backend<'_>, Error> {
    let mut reader = Reader(body);
    let message = match tag {
        b'R' => Backend::Authentication(match reader.u32()? {
            0 => Authentication::Ok,
            10 => {
                let names = reader.0;
                loop {
                    if reader.cstr()?.is_empty() {
                        break;
                    }
                }
                Authentication::Sasl(Mechanisms(names))
            }
            11 => Authentication::SaslContinue(reader.rest()),
            12 => Authentication::SaslFinal(reader.rest()),
            _ => {
                // Another method's body is not read: it is refused whole.
                reader.rest();
                Authentication::Other
            }
        }),
        b'K' => {
            let _process = reader.i32()?;
            let _secret = reader.i32()?;
            Backend::KeyData
        }
        b'2' => Backend::BindComplete,
        b'C' => Backend::CommandComplete(reader.cstr()?),
        b'D' => return DataRow::parse(body).map(Backend::DataRow),
        b'I' => Backend::EmptyQueryResponse,
        b'E' => return error_response(body).map(Backend::ErrorResponse),
        b'n' => Backend::NoData,
        b'N' => {
            fields(&mut reader)?;
            Backend::NoticeResponse
        }
        b'A' => {
            let process_id = reader.i32()?;
            let channel = utf8(reader.cstr()?)?;
            let payload = utf8(reader.cstr()?)?;
            Backend::Notification(Notice {
                process_id,
                channel,
                payload,
            })
        }
        b'S' => {
            let name = reader.cstr()?;
            let value = reader.cstr()?;
            Backend::ParameterStatus { name, value }
        }
        b'1' => Backend::ParseComplete,
        b'Z' => Backend::ReadyForQuery(match reader.u8()? {
            b'I' => TransactionStatus::Idle,
            b'T' => TransactionStatus::InTransaction,
            b'E' => TransactionStatus::Failed,
            _ => return Err(Error::Protocol),
        }),
        b'T' => return RowDescription::parse(body).map(Backend::RowDescription),
        // CopyInResponse, CopyOutResponse, CopyBothResponse, CopyData,
        // CopyDone, FunctionCallResponse, ParameterDescription,
        // PortalSuspended, CloseComplete, NegotiateProtocolVersion.
        b'G' | b'H' | b'W' | b'd' | b'c' | b'V' | b't' | b's' | b'3' | b'v' => {
            reader.rest();
            Backend::Unrequested
        }
        _ => return Err(Error::Protocol),
    };
    reader.end()?;
    Ok(message)
}

fn utf8(bytes: &[u8]) -> Result<&str, Error> {
    std::str::from_utf8(bytes).map_err(|_| Error::Protocol)
}

/// The fields every ErrorResponse and NoticeResponse carries.
struct Fields<'a> {
    severity: Option<&'a [u8]>,
    localized: Option<&'a [u8]>,
    code: Option<&'a [u8]>,
}

/// Walk an error or notice's fields to its terminator. A severity or code
/// given twice is refused, so no later field can override the first.
fn fields<'a>(reader: &mut Reader<'a>) -> Result<Fields<'a>, Error> {
    let mut found = Fields {
        severity: None,
        localized: None,
        code: None,
    };
    loop {
        let kind = reader.u8()?;
        if kind == 0 {
            return Ok(found);
        }
        let text = reader.cstr()?;
        let slot = match kind {
            b'V' => &mut found.severity,
            b'S' => &mut found.localized,
            b'C' => &mut found.code,
            _ => continue,
        };
        if slot.is_some() {
            return Err(Error::Protocol);
        }
        *slot = Some(text);
    }
}

fn error_response(body: &[u8]) -> Result<DbError, Error> {
    let mut reader = Reader(body);
    let fields = fields(&mut reader)?;
    reader.end()?;
    // V is never localized; S is, and is read only where V is absent.
    let severity = match fields.severity.or(fields.localized) {
        Some(b"ERROR") => Severity::Error,
        Some(b"FATAL") => Severity::Fatal,
        Some(b"PANIC") => Severity::Panic,
        _ => return Err(Error::Protocol),
    };
    let code: &[u8; 5] = fields
        .code
        .and_then(|code| code.try_into().ok())
        .ok_or(Error::Protocol)?;
    let code = SqlState::new(code).ok_or(Error::Protocol)?;
    Ok(DbError::new(severity, code))
}

/// A cursor over a message body; every read is checked.
pub(crate) struct Reader<'a>(pub(crate) &'a [u8]);

impl<'a> Reader<'a> {
    pub(crate) fn take(&mut self, length: usize) -> Result<&'a [u8], Error> {
        let (taken, rest) = self.0.split_at_checked(length).ok_or(Error::Protocol)?;
        self.0 = rest;
        Ok(taken)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        self.take(N)?.try_into().map_err(|_| Error::Protocol)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, Error> {
        Ok(u8::from_be_bytes(self.array()?))
    }

    pub(crate) fn i16(&mut self) -> Result<i16, Error> {
        Ok(i16::from_be_bytes(self.array()?))
    }

    pub(crate) fn i32(&mut self) -> Result<i32, Error> {
        Ok(i32::from_be_bytes(self.array()?))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    /// The bytes before the next NUL, which is consumed.
    pub(crate) fn cstr(&mut self) -> Result<&'a [u8], Error> {
        let end = self
            .0
            .iter()
            .position(|&byte| byte == 0)
            .ok_or(Error::Protocol)?;
        let text = self.take(end)?;
        self.take(1)?;
        Ok(text)
    }

    pub(crate) fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.0)
    }

    pub(crate) fn end(&self) -> Result<(), Error> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(Error::Protocol)
        }
    }
}

// ---- Reading whole messages ---------------------------------------------

/// The read buffer. Bytes past a returned message stay buffered, and a
/// read that times out keeps what it had, so an idle wait can stop
/// mid-message and resume.
#[derive(Debug, Default)]
pub(crate) struct Inbox {
    buffer: Vec<u8>,
    start: usize,
    end: usize,
}

impl Inbox {
    /// The next whole message's tag and body bounds, for `body`. `None`
    /// when the deadline passed first: without `poll`, a passed deadline
    /// reads nothing more; with it, what is ready is still read.
    pub(crate) fn next<S: Stream>(
        &mut self,
        stream: &mut S,
        limit: usize,
        deadline: Instant,
        poll: bool,
    ) -> Result<Option<(u8, usize, usize)>, Error> {
        loop {
            let available = self.end.saturating_sub(self.start);
            let need = match self.buffer.get(self.start..self.end) {
                Some([tag, a, b, c, d, ..]) => {
                    let length = i32::from_be_bytes([*a, *b, *c, *d]);
                    let body = usize::try_from(length)
                        .ok()
                        .and_then(|length| length.checked_sub(4))
                        .ok_or(Error::Protocol)?;
                    if body > limit {
                        return Err(Error::Limit);
                    }
                    let total = body.checked_add(5).ok_or(Error::Limit)?;
                    if available >= total {
                        let tag = *tag;
                        let first = self.start + 5;
                        self.start += total;
                        return Ok(Some((tag, first, first + body)));
                    }
                    total
                }
                Some(_) => 5,
                None => return Err(Error::Protocol),
            };
            if !poll && crate::stream::remaining(deadline).is_none() {
                return Ok(None);
            }
            self.make_room(need, limit)?;
            let space = self.buffer.get_mut(self.end..).ok_or(Error::Protocol)?;
            match stream.read(space, deadline) {
                Ok(0) => return Err(Error::Closed),
                Ok(count) if count <= space.len() => self.end += count,
                Ok(_) => return Err(Error::Io),
                Err(StreamError::TimedOut) => return Ok(None),
                Err(StreamError::Failed) => return Err(Error::Io),
            }
        }
    }

    /// Make the buffer hold `need` bytes from the start of the pending
    /// message, which `need` exceeds what is buffered of.
    fn make_room(&mut self, need: usize, limit: usize) -> Result<(), Error> {
        if self.start == self.end {
            self.start = 0;
            self.end = 0;
        }
        if self.start.checked_add(need).ok_or(Error::Limit)? <= self.buffer.len() {
            return Ok(());
        }
        if self.start > 0 {
            self.buffer.copy_within(self.start..self.end, 0);
            self.end -= self.start;
            self.start = 0;
        }
        if need > self.buffer.len() {
            let ceiling = limit.checked_add(5).ok_or(Error::Limit)?;
            let size = need.max(FIRST_BUFFER.min(ceiling));
            td_alloc::resize(&mut self.buffer, size, 0)?;
        }
        Ok(())
    }

    pub(crate) fn body(&self, start: usize, end: usize) -> &[u8] {
        self.buffer.get(start..end).unwrap_or(&[])
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unreachable
)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn frame(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut bytes = vec![tag];
        bytes.extend_from_slice(&(body.len() as u32 + 4).to_be_bytes());
        bytes.extend_from_slice(body);
        bytes
    }

    /// Bytes served a few at a time, then EOF.
    pub(crate) struct Script {
        pub(crate) input: Vec<u8>,
        pub(crate) at: usize,
        pub(crate) chunk: usize,
        pub(crate) written: Vec<u8>,
        /// Time out instead of closing once the input is spent.
        pub(crate) stall: bool,
        /// Fail every write, as to a peer that has closed.
        pub(crate) refuse_writes: bool,
    }

    impl Script {
        pub(crate) fn new(input: Vec<u8>, chunk: usize) -> Self {
            Self {
                input,
                at: 0,
                chunk,
                written: Vec::new(),
                stall: false,
                refuse_writes: false,
            }
        }
    }

    impl Stream for Script {
        fn read(&mut self, buffer: &mut [u8], _: Instant) -> Result<usize, StreamError> {
            let rest = &self.input[self.at..];
            if rest.is_empty() && self.stall {
                return Err(StreamError::TimedOut);
            }
            let count = rest.len().min(buffer.len()).min(self.chunk);
            buffer[..count].copy_from_slice(&rest[..count]);
            self.at += count;
            Ok(count)
        }

        fn write_all(&mut self, bytes: &[u8], _: Instant) -> Result<(), StreamError> {
            if self.refuse_writes {
                return Err(StreamError::Failed);
            }
            self.written.extend_from_slice(bytes);
            Ok(())
        }
    }

    fn later() -> Instant {
        Instant::now() + std::time::Duration::from_secs(5)
    }

    fn read_all(input: Vec<u8>, chunk: usize, limit: usize) -> Result<Vec<(u8, Vec<u8>)>, Error> {
        let mut stream = Script::new(input, chunk);
        let mut inbox = Inbox::default();
        let mut out = Vec::new();
        loop {
            match inbox.next(&mut stream, limit, later(), false) {
                Ok(Some((tag, start, end))) => out.push((tag, inbox.body(start, end).to_vec())),
                Ok(None) => unreachable!("the script never times out"),
                Err(Error::Closed) => return Ok(out),
                Err(error) => return Err(error),
            }
        }
    }

    #[test]
    fn messages_survive_every_fragmentation() {
        let mut bytes = frame(b'Z', b"I");
        bytes.extend(frame(b'D', &[7; 20_000]));
        bytes.extend(frame(b'C', b"SELECT 1\0"));
        bytes.extend(frame(b'n', b""));
        for chunk in [1, 2, 4, 5, 6, 9, 1000, 8192, 100_000] {
            let messages = read_all(bytes.clone(), chunk, 32 * 1024).unwrap();
            assert_eq!(messages.len(), 4, "chunk {chunk}");
            assert_eq!(messages[0], (b'Z', b"I".to_vec()));
            assert_eq!(messages[1], (b'D', vec![7; 20_000]));
            assert_eq!(messages[2].1, b"SELECT 1\0");
            assert_eq!(messages[3], (b'n', Vec::new()));
        }
    }

    #[test]
    fn a_length_below_four_or_above_the_limit_is_refused_before_buffering() {
        for length in [0u32, 3, 0x8000_0000, u32::MAX] {
            let mut bytes = vec![b'D'];
            bytes.extend_from_slice(&length.to_be_bytes());
            assert_eq!(read_all(bytes, 64, 1024), Err(Error::Protocol), "{length}");
        }
        let mut bytes = vec![b'D'];
        bytes.extend_from_slice(&(1024u32 + 5).to_be_bytes());
        assert_eq!(read_all(bytes, 64, 1024), Err(Error::Limit));
        let mut inbox = Inbox::default();
        let mut stream = Script::new(frame(b'D', &[0; 1024]), 4096);
        assert!(inbox
            .next(&mut stream, 1024, later(), false)
            .unwrap()
            .is_some());
        assert!(inbox.buffer.len() <= 8 * 1024);
    }

    #[test]
    fn a_stalled_read_keeps_its_partial_message() {
        let bytes = frame(b'C', b"COMMIT\0");
        let mut stream = Script::new(bytes[..6].to_vec(), 100);
        stream.stall = true;
        let mut inbox = Inbox::default();
        assert_eq!(inbox.next(&mut stream, 1024, later(), true), Ok(None));
        stream.input.extend_from_slice(&bytes[6..]);
        let (tag, start, end) = inbox
            .next(&mut stream, 1024, later(), true)
            .unwrap()
            .unwrap();
        assert_eq!((tag, inbox.body(start, end)), (b'C', &b"COMMIT\0"[..]));
        // Without poll, a passed deadline reads nothing.
        let mut stream = Script::new(bytes, 100);
        assert_eq!(
            inbox.next(&mut stream, 1024, Instant::now(), false),
            Ok(None)
        );
        assert_eq!(stream.at, 0);
    }

    #[test]
    fn every_backend_message_reads_and_trailing_bytes_are_refused() {
        let cases: &[(u8, &[u8])] = &[
            (b'R', &[0, 0, 0, 0]),
            (b'R', b"\0\0\0\x0aSCRAM-SHA-256\0\0"),
            (b'R', b"\0\0\0\x0bdata"),
            (b'R', b"\0\0\0\x0cdata"),
            (b'R', &[0, 0, 0, 5, 1, 2, 3, 4]),
            (b'K', &[0, 0, 0, 1, 0, 0, 0, 2]),
            (b'2', b""),
            (b'C', b"INSERT 0 1\0"),
            (b'D', &[0, 2, 255, 255, 255, 255, 0, 0, 0, 1, 9]),
            (b'I', b""),
            (b'E', b"VERROR\0C23505\0Mdup\0\0"),
            (b'n', b""),
            (b'N', b"SNOTICE\0Mhi\0\0"),
            (b'A', b"\0\0\0\x07ch\0pay\0"),
            (b'S', b"client_encoding\0UTF8\0"),
            (b'1', b""),
            (b'Z', b"T"),
            (
                b'T',
                b"\0\x01id\0\0\0\0\0\0\0\0\0\0\x14\0\x08\xff\xff\xff\xff\0\x01",
            ),
        ];
        for (tag, body) in cases {
            assert!(backend(*tag, body).is_ok(), "{}", *tag as char);
            let mut longer = body.to_vec();
            longer.push(0);
            if *tag != b'R' || !matches!(body.get(3), Some(11 | 12 | 5)) {
                assert_eq!(
                    backend(*tag, &longer).err(),
                    Some(Error::Protocol),
                    "{}",
                    *tag as char
                );
            }
            for cut in 0..body.len() {
                let short = &body[..cut];
                // Only a message that may legitimately be shorter passes.
                if let Ok(message) = backend(*tag, short) {
                    assert!(
                        matches!(
                            message,
                            Backend::Authentication(
                                Authentication::SaslContinue(_)
                                    | Authentication::SaslFinal(_)
                                    | Authentication::Other
                            )
                        ),
                        "{} cut at {cut}",
                        *tag as char
                    );
                }
            }
        }
    }

    #[test]
    fn hostile_values_are_refused() {
        // A NULL is -1; any other negative length is refused.
        assert!(backend(b'D', &[0, 1, 255, 255, 255, 254]).is_err());
        // A value longer than the message.
        assert!(backend(b'D', &[0, 1, 0, 0, 0, 9, 1]).is_err());
        // A negative column count.
        assert!(backend(b'D', &[255, 255]).is_err());
        assert!(backend(b'T', &[128, 0]).is_err());
        // A column count the bytes do not back.
        assert!(backend(b'D', &[0, 3, 0, 0, 0, 0]).is_err());
        // A described format other than text or binary.
        assert!(backend(
            b'T',
            b"\0\x01id\0\0\0\0\0\0\0\0\0\0\x14\0\x08\xff\xff\xff\xff\0\x02"
        )
        .is_err());
        // An unknown transaction status, tag or SQLSTATE.
        assert!(backend(b'Z', b"X").is_err());
        assert!(backend(b'?', b"").is_err());
        for bad in [
            &b"VERROR\0C2350\0\0"[..],
            b"VERROR\0C23505x\0\0",
            b"VERROR\0Cabcde\0\0",
            b"VERROR\0\0",
            b"C23505\0\0",
            b"VWARNING\0C01000\0\0",
            b"VERROR\0C23505\0VFATAL\0\0",
            b"VERROR\0C23505\0C40001\0\0",
            b"VERROR\0C23505\0",
            b"VERROR\0C23505",
        ] {
            assert_eq!(backend(b'E', bad).err(), Some(Error::Protocol), "{bad:?}");
        }
        // The localized severity stands in only when V is absent.
        assert!(matches!(
            backend(b'E', b"SFATAL\0C57P01\0\0"),
            Ok(Backend::ErrorResponse(error)) if error.severity() == Severity::Fatal
        ));
        assert!(matches!(
            backend(b'E', b"SFEHLER\0VERROR\0C42601\0\0"),
            Ok(Backend::ErrorResponse(error)) if error.severity() == Severity::Error
        ));
        // A mechanism list without its terminator, a channel not UTF-8.
        assert!(backend(b'R', b"\0\0\0\x0aSCRAM-SHA-256\0").is_err());
        assert!(backend(b'A', b"\0\0\0\x07\xff\0\0").is_err());
    }

    #[test]
    fn mechanisms_match_whole_names_only() {
        let Ok(Backend::Authentication(Authentication::Sasl(list))) =
            backend(b'R', b"\0\0\0\x0aSCRAM-SHA-256-PLUS\0\0")
        else {
            panic!("not a SASL request");
        };
        assert!(!list.offers(b"SCRAM-SHA-256"));
        assert!(list.offers(b"SCRAM-SHA-256-PLUS"));
    }

    #[test]
    fn rows_and_descriptions_iterate_what_was_checked() {
        let Ok(Backend::DataRow(row)) = backend(b'D', &[0, 2, 255, 255, 255, 255, 0, 0, 0, 1, 9])
        else {
            panic!("not a row");
        };
        assert_eq!(row.len(), 2);
        assert_eq!(row.values().collect::<Vec<_>>(), [None, Some(&[9][..])]);
        let Ok(Backend::RowDescription(description)) = backend(
            b'T',
            b"\0\x02a\0\0\0\0\0\0\0\0\0\0\x14\0\x08\xff\xff\xff\xff\0\x01b\0\0\0\0\0\0\0\0\0\0\x11\xff\xff\xff\xff\xff\xff\0\x00",
        ) else {
            panic!("not a description");
        };
        let fields: Vec<_> = description.fields().collect();
        assert_eq!(fields.len(), 2);
        assert_eq!(
            (fields[0].name, fields[0].type_oid, fields[0].format_code),
            (&b"a"[..], 20, 1)
        );
        assert_eq!(
            (fields[1].name, fields[1].type_oid, fields[1].format_code),
            (&b"b"[..], 17, 0)
        );
    }

    #[test]
    fn frontend_messages_have_their_exact_bytes() {
        let mut out = Vec::new();
        startup(&mut out, &[("user", "td"), ("database", "mail")]).unwrap();
        assert_eq!(out, b"\0\0\0\x1f\0\x03\0\0user\0td\0database\0mail\0\0");
        out.clear();
        query(&mut out, "BEGIN").unwrap();
        assert_eq!(out, b"Q\0\0\0\x0aBEGIN\0");
        out.clear();
        sasl_initial(&mut out, b"SCRAM-SHA-256", b"n,,n=,r=x").unwrap();
        assert_eq!(out, b"p\0\0\0\x1fSCRAM-SHA-256\0\0\0\0\x09n,,n=,r=x");
        out.clear();
        parse(&mut out, "SELECT $1", [20u32].into_iter()).unwrap();
        assert_eq!(out, b"P\0\0\0\x15\0SELECT $1\0\0\x01\0\0\0\x14");
        out.clear();
        describe_execute_sync(&mut out).unwrap();
        assert_eq!(out, b"D\0\0\0\x06P\0E\0\0\0\x09\0\0\0\0\0S\0\0\0\x04");
        assert!(query(&mut Vec::new(), "SELECT '\0'").is_err());
        assert!(startup(&mut Vec::new(), &[("a", &"x".repeat(10_000))]).is_err());
    }
}
