//! Root-header retention for one receiving delivery; body bytes stay streamed.
use crate::{
    admission::work::Meter,
    ports::{Clock, Deadline, Error, Tick},
};
use crate::{
    format::row::{ReceiptTls, SmtpReceipt},
    ids::EmailId,
    ports,
};
use std::{fmt::Write as _, net::IpAddr};
use td_mime::{header_message_ids as ids, headers};

const OUTPUT_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HeaderError {
    HeaderLimit,
    Work(Error),
}
impl From<Error> for HeaderError {
    fn from(error: Error) -> Self {
        Self::Work(error)
    }
}

pub(crate) struct Work<'a> {
    pub clock: &'a dyn Clock,
    pub deadline: Deadline,
    pub meter: &'a mut Meter,
    pub last: &'a mut Tick,
}
impl Work<'_> {
    pub fn tick(&mut self) -> Result<td_mime::time::Tick, Error> {
        let now = self.clock.sample()?.monotonic;
        if now < *self.last {
            return Err(Error::Invalid);
        }
        *self.last = now;
        if self.deadline.expired(now) {
            return Err(Error::Deadline);
        }
        Ok(td_mime::time::Tick(now.0))
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Candidate {
    pub start: usize,
    pub end: usize,
    pub length: usize,
}
#[derive(Clone, Copy, Default)]
pub(crate) struct Ring {
    items: [Option<Candidate>; 32],
    next: usize,
    count: usize,
}
impl Ring {
    fn push(&mut self, item: Candidate) -> Result<(), Error> {
        *self.items.get_mut(self.next).ok_or(Error::Invalid)? = Some(item);
        self.next = (self.next + 1) % 32;
        self.count = (self.count + 1).min(32);
        Ok(())
    }
    pub fn newest(&self, index: usize) -> Option<Candidate> {
        if index >= self.count {
            return None;
        }
        self.items
            .get((self.next + 31 - index) % 32)
            .copied()
            .flatten()
    }
    fn append(&mut self, later: &Self) -> Result<(), Error> {
        for n in (0..later.count).rev() {
            self.push(later.newest(n).ok_or(Error::Invalid)?)?;
        }
        Ok(())
    }
}
#[derive(Default)]
pub(crate) struct Identifiers {
    pub own: Option<Candidate>,
    pub references: Ring,
    pub reply: Ring,
}
impl Identifiers {
    pub fn field(
        &mut self,
        source: &[u8],
        field: headers::Field,
        work: &mut Work<'_>,
    ) -> Result<(), Error> {
        let name = extent(source, field.name_start, field.name_end)?;
        let own = name.eq_ignore_ascii_case(b"message-id");
        let references = name.eq_ignore_ascii_case(b"references");
        let reply = name.eq_ignore_ascii_case(b"in-reply-to");
        if !(references || reply || (own && self.own.is_none())) {
            return Ok(());
        }
        let start = usize::try_from(field.value_start).map_err(|_| Error::Invalid)?;
        let end = usize::try_from(field.value_end).map_err(|_| Error::Invalid)?;
        let value = source.get(start..end).ok_or(Error::Invalid)?;
        let mode = if own {
            ids::Mode::Strict
        } else {
            ids::Mode::ObsoletePhrases
        };
        let mut cursor = ids::Cursor::new(value, mode);
        let mut first = None;
        let mut ring = Ring::default();
        let mut identifier_start = 0usize;
        let mut length = 0usize;
        loop {
            let now = work.tick()?;
            let status = match cursor.poll(now, work.meter) {
                Ok(status) => status,
                Err(ids::Error::Malformed | ids::Error::NestingLimit) => return Ok(()),
                Err(error) => return Err(id_error(error)),
            };
            match status {
                ids::Status::Yield => {}
                ids::Status::Begin => {
                    length = 0;
                    identifier_start = start
                        .checked_add(
                            cursor
                                .source_position()
                                .checked_sub(1)
                                .ok_or(Error::Invalid)?,
                        )
                        .ok_or(Error::Capacity)?;
                }
                ids::Status::Part(part) => {
                    length = length
                        .checked_add(part.end.checked_sub(part.start).ok_or(Error::Invalid)?)
                        .ok_or(Error::Capacity)?;
                }
                ids::Status::End => {
                    let candidate = Candidate {
                        start: identifier_start,
                        end: start
                            .checked_add(cursor.source_position())
                            .ok_or(Error::Capacity)?,
                        length,
                    };
                    first.get_or_insert(candidate);
                    ring.push(candidate)?;
                }
                ids::Status::Complete => break,
            }
        }
        if own {
            self.own = first;
        } else if references {
            self.references.append(&ring)?;
        } else {
            self.reply.append(&ring)?;
        }
        Ok(())
    }
}

pub(crate) fn candidate<'a>(
    source: &[u8],
    item: Candidate,
    output: &'a mut [u8],
    work: &mut Work<'_>,
) -> Result<Option<&'a str>, Error> {
    if item.length > crate::format::key::MAX_ANCHOR_BYTES {
        return Ok(None);
    }
    let source = source.get(item.start..item.end).ok_or(Error::Invalid)?;
    let mut cursor = ids::Cursor::new(source, ids::Mode::Strict);
    let mut used = 0usize;
    loop {
        let now = work.tick()?;
        match cursor.poll(now, work.meter).map_err(id_error)? {
            ids::Status::Yield | ids::Status::Begin | ids::Status::End => {}
            ids::Status::Part(part) => {
                let bytes = source.get(part.start..part.end).ok_or(Error::Invalid)?;
                let end = used.checked_add(bytes.len()).ok_or(Error::Capacity)?;
                output
                    .get_mut(used..end)
                    .ok_or(Error::Capacity)?
                    .copy_from_slice(bytes);
                used = end;
            }
            ids::Status::Complete => {
                if used != item.length {
                    return Err(Error::Invalid);
                }
                return std::str::from_utf8(output.get(..used).ok_or(Error::Invalid)?)
                    .map(Some)
                    .map_err(|_| Error::Invalid);
            }
        }
    }
}

fn id_error(error: ids::Error) -> Error {
    match error {
        ids::Error::Work(td_mime::work::Stop::Deadline) => Error::Deadline,
        ids::Error::NestingLimit | ids::Error::Work(_) | ids::Error::InterpretationLimit => {
            Error::Capacity
        }
        ids::Error::Malformed | ids::Error::InvalidState => Error::Invalid,
    }
}
pub(crate) fn extent(source: &[u8], start: u64, end: u64) -> Result<&[u8], Error> {
    source
        .get(
            usize::try_from(start).map_err(|_| Error::Invalid)?
                ..usize::try_from(end).map_err(|_| Error::Invalid)?,
        )
        .ok_or(Error::Invalid)
}

pub(crate) struct Headers {
    pub source: Vec<u8>,
    pub identifiers: Identifiers,
    scanner: headers::Scanner,
    position: usize,
    limit: usize,
    end: Option<headers::End>,
}
impl Headers {
    pub fn new(limit: usize) -> Result<Self, HeaderError> {
        let capacity = limit.checked_add(1000).ok_or(Error::Capacity)?;
        let mut source = Vec::new();
        source
            .try_reserve_exact(capacity)
            .map_err(|_| Error::Capacity)?;
        Ok(Self {
            source,
            identifiers: Identifiers::default(),
            scanner: headers::Scanner::new(0, limit as u64),
            position: 0,
            limit,
            end: None,
        })
    }
    pub fn complete(&self) -> bool {
        self.end.is_some()
    }
    pub fn push(
        &mut self,
        input: &[u8],
        last: bool,
        work: &mut Work<'_>,
    ) -> Result<bool, HeaderError> {
        if self.complete() || input.len() > 1000 {
            return Err(Error::Invalid.into());
        }
        let next = self
            .source
            .len()
            .checked_add(input.len())
            .ok_or(Error::Capacity)?;
        if next > self.limit.checked_add(1000).ok_or(Error::Capacity)? {
            return Err(HeaderError::HeaderLimit);
        }
        self.source.extend_from_slice(input);
        loop {
            let now = work.tick()?;
            let progress = self
                .scanner
                .poll(
                    self.source.get(self.position..).ok_or(Error::Invalid)?,
                    last,
                    now,
                    work.meter,
                )
                .map_err(header_error)?;
            self.position = self
                .position
                .checked_add(progress.consumed)
                .ok_or(Error::Capacity)?;
            match progress.status {
                headers::Status::Field(field) => {
                    self.identifiers.field(&self.source, field, work)?
                }
                headers::Status::Yield => {}
                headers::Status::NeedInput => return Ok(false),
                headers::Status::Complete(end) => {
                    self.end = Some(end);
                    return Ok(true);
                }
            }
        }
    }
    /// Shared nearest-first thread evidence from original root headers.
    pub fn thread_identifiers(&self, work: &mut Work<'_>) -> Result<(Vec<String>, String), Error> {
        let mut anchors = Vec::new();
        anchors.try_reserve_exact(65).map_err(|_| Error::Capacity)?;
        let mut scratch = [0; crate::format::key::MAX_ANCHOR_BYTES];
        for ring in [&self.identifiers.references, &self.identifiers.reply] {
            for index in 0..32 {
                let Some(item) = ring.newest(index) else {
                    break;
                };
                if let Some(id) = candidate(&self.source, item, &mut scratch, work)? {
                    anchors.push(copy_identifier(id, work)?);
                }
            }
        }
        let own = self
            .identifiers
            .own
            .map(|item| {
                candidate(&self.source, item, &mut scratch, work)?
                    .map(|id| copy_identifier(id, work))
                    .transpose()
            })
            .transpose()?
            .flatten()
            .unwrap_or_default();
        if !own.is_empty() {
            anchors.push(copy_identifier(&own, work)?);
        }
        Ok((anchors, own))
    }
    pub fn write_filtered(
        &self,
        trace: &[u8],
        mut write: impl FnMut(&[u8]) -> Result<(), Error>,
        work: &mut Work<'_>,
    ) -> Result<(), HeaderError> {
        let expected = self.end.ok_or(Error::Invalid)?;
        let mut scanner = headers::Scanner::new(0, self.limit as u64);
        let mut position = 0usize;
        let mut retained = trace.len();
        if retained > self.limit {
            return Err(HeaderError::HeaderLimit);
        }
        emit(trace, &mut write, work)?;
        loop {
            let now = work.tick()?;
            let progress = scanner
                .poll(
                    self.source.get(position..).ok_or(Error::Invalid)?,
                    true,
                    now,
                    work.meter,
                )
                .map_err(header_error)?;
            position = position
                .checked_add(progress.consumed)
                .ok_or(Error::Capacity)?;
            match progress.status {
                headers::Status::Field(field) => {
                    let name = extent(&self.source, field.name_start, field.name_end)?;
                    if !name.eq_ignore_ascii_case(b"return-path") {
                        let end = field.value_end.checked_add(2).ok_or(Error::Capacity)?;
                        let bytes = extent(&self.source, field.name_start, end)?;
                        if !bytes.ends_with(b"\r\n") {
                            return Err(Error::Invalid.into());
                        }
                        retained = retained.checked_add(bytes.len()).ok_or(Error::Capacity)?;
                        if retained > self.limit {
                            return Err(HeaderError::HeaderLimit);
                        }
                        emit(bytes, &mut write, work)?;
                    }
                }
                headers::Status::Yield => {}
                headers::Status::NeedInput => return Err(Error::Invalid.into()),
                headers::Status::Complete(end) => {
                    if end != expected {
                        return Err(Error::Invalid.into());
                    }
                    let tail = self
                        .source
                        .get(usize::try_from(end.header_bytes).map_err(|_| Error::Invalid)?..)
                        .ok_or(Error::Invalid)?;
                    // An orphan continuation was body in the received message.
                    // Keep it from continuing our newly prepended Received field.
                    if end.header_bytes == 0 && matches!(tail.first(), Some(b' ' | b'\t')) {
                        emit(b"\r\n", &mut write, work)?;
                    }
                    emit(tail, &mut write, work)?;
                    return Ok(());
                }
            }
        }
    }
}
fn header_error(error: headers::Error) -> HeaderError {
    match error {
        headers::Error::HeaderLimit => HeaderError::HeaderLimit,
        headers::Error::Work(td_mime::work::Stop::Deadline) => Error::Deadline.into(),
        headers::Error::Work(_) | headers::Error::InterpretationLimit => Error::Capacity.into(),
        headers::Error::Offset | headers::Error::InvalidState => Error::Invalid.into(),
    }
}

fn emit(
    bytes: &[u8],
    write: &mut impl FnMut(&[u8]) -> Result<(), Error>,
    work: &mut Work<'_>,
) -> Result<(), Error> {
    for chunk in bytes.chunks(OUTPUT_CHUNK_BYTES) {
        let now = work.tick()?;
        work.meter
            .charge(
                now,
                td_mime::work::Charge {
                    io_bytes: chunk.len() as u64,
                    output_bytes: chunk.len() as u64,
                    records: 0,
                    unlinks: 0,
                },
            )
            .map_err(|e| {
                if e == td_mime::work::Stop::Deadline {
                    Error::Deadline
                } else {
                    Error::Capacity
                }
            })?;
        write(chunk)?;
    }
    Ok(())
}

pub(crate) fn trace_allowance(hostname: &str) -> Result<usize, ports::Error> {
    crate::config::values::certificate_name(hostname).map_err(|_| ports::Error::Invalid)?;
    // Each variable has a protocol bound; 128 covers every fixed delimiter,
    // the longest protocol/TLS comment and UTC date; two more bytes protect
    // an orphan continuation with an explicit header/body separator.
    hostname
        .len()
        .checked_add(crate::format::row::MAX_EHLO)
        .and_then(|n| n.checked_add(crate::format::row::MAX_ADDRESS))
        .and_then(|n| n.checked_add(52 + 32 + 128 + 2))
        .ok_or(ports::Error::Capacity)
}
pub(crate) fn trace(
    receipt: &SmtpReceipt<'_>,
    hostname: &str,
    extended: bool,
    email: EmailId,
    utc_ms: i64,
    maximum: usize,
) -> Result<String, ports::Error> {
    let date = td_civil::unix_to_civil_utc_checked(utc_ms.div_euclid(1000))
        .ok_or(ports::Error::Invalid)?;
    if !(1900..=9999).contains(&date.year) {
        return Err(ports::Error::Invalid);
    }
    const MONTHS: &[&str] = &[
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = MONTHS
        .get(
            usize::from(date.month)
                .checked_sub(1)
                .ok_or(ports::Error::Invalid)?,
        )
        .ok_or(ports::Error::Invalid)?;
    let protocol = match (extended, receipt.tls) {
        (true, ReceiptTls::Plain) => "ESMTP",
        (true, _) => "ESMTPS",
        (false, _) => "SMTP",
    };
    let tls = match receipt.tls {
        ReceiptTls::Plain => "",
        ReceiptTls::Tls12 => " (TLSv1.2)",
        ReceiptTls::Tls13 => " (TLSv1.3)",
    };
    let mut output = String::new();
    output
        .try_reserve_exact(maximum)
        .map_err(|_| ports::Error::Capacity)?;
    write!(
        output,
        "Return-Path: <{}>\r\nReceived: from {} (",
        receipt.reverse_path, receipt.ehlo
    )
    .map_err(|_| ports::Error::Capacity)?;
    match receipt.peer {
        IpAddr::V4(ip) => write!(output, "[{ip}]"),
        IpAddr::V6(ip) => write!(output, "[IPv6:{ip}]"),
    }
    .map_err(|_| ports::Error::Capacity)?;
    write!(output, ")\r\n\tby {hostname} with {protocol}{tls} id {email};\r\n\t{} {month} {} {:02}:{:02}:{:02} +0000\r\n",
        date.day, date.year, date.hour, date.minute, date.second).map_err(|_| ports::Error::Capacity)?;
    if output.len() > maximum {
        return Err(ports::Error::Capacity);
    }
    Ok(output)
}

fn copy_identifier(id: &str, work: &mut Work<'_>) -> Result<String, Error> {
    let now = work.tick()?;
    work.meter
        .charge(
            now,
            td_mime::work::Charge {
                io_bytes: id.len() as u64,
                output_bytes: id.len() as u64,
                records: 1,
                unlinks: 0,
            },
        )
        .map_err(|e| {
            if e == td_mime::work::Stop::Deadline {
                Error::Deadline
            } else {
                Error::Capacity
            }
        })?;
    let mut copy = String::new();
    copy.try_reserve_exact(id.len())
        .map_err(|_| Error::Capacity)?;
    copy.push_str(id);
    Ok(copy)
}
