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
use std::{borrow::Cow, fmt::Write as _, net::IpAddr};
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

pub(crate) struct Headers<'a> {
    source: Cow<'a, [u8]>,
    available: usize,
    pub identifiers: Identifiers,
    scanner: headers::Scanner,
    position: usize,
    limit: usize,
    end: Option<headers::End>,
}
impl<'a> Headers<'a> {
    pub fn new(limit: usize) -> Result<Self, HeaderError> {
        let capacity = limit.checked_add(1000).ok_or(Error::Capacity)?;
        let mut source = Vec::new();
        source
            .try_reserve_exact(capacity)
            .map_err(|_| Error::Capacity)?;
        Ok(Self {
            source: Cow::Owned(source),
            available: 0,
            identifiers: Identifiers::default(),
            scanner: headers::Scanner::new(0, limit as u64),
            position: 0,
            limit,
            end: None,
        })
    }
    /// Borrow already admitted immutable DATA instead of retaining tentative
    /// field-name bytes in the header arena. Only recognized fields spend its cap.
    pub fn resident(source: &'a [u8], limit: usize) -> Self {
        Self {
            source: Cow::Borrowed(source),
            available: 0,
            identifiers: Identifiers::default(),
            scanner: headers::Scanner::new(0, limit as u64),
            position: 0,
            limit,
            end: None,
        }
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
            .available
            .checked_add(input.len())
            .ok_or(Error::Capacity)?;
        match &mut self.source {
            Cow::Owned(source) => {
                if next > self.limit.checked_add(1000).ok_or(Error::Capacity)? {
                    return Err(HeaderError::HeaderLimit);
                }
                source.extend_from_slice(input);
            }
            Cow::Borrowed(source) => {
                let part = source.get(self.available..next).ok_or(Error::Invalid)?;
                // The resident path advances only through that original source.
                if !input.is_empty() && !std::ptr::eq(part, input) {
                    return Err(Error::Invalid.into());
                }
            }
        }
        self.available = next;
        self.scan(last, work)
    }
    fn scan(&mut self, last: bool, work: &mut Work<'_>) -> Result<bool, HeaderError> {
        loop {
            let now = work.tick()?;
            let progress = self
                .scanner
                .poll(
                    self.source
                        .get(self.position..self.available)
                        .ok_or(Error::Invalid)?,
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
                    self.source
                        .get(position..self.available)
                        .ok_or(Error::Invalid)?,
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
                        .get(
                            usize::try_from(end.header_bytes).map_err(|_| Error::Invalid)?
                                ..self.available,
                        )
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
/// Upper bound including RFC 7208 §9.1. A quoted byte can double; a null
/// reverse path supplies postmaster@ followed by the greeting a second time.
pub(crate) fn spf_trace_allowance(hostname: &str) -> Result<usize, ports::Error> {
    let frame = b"Received-SPF: permerror identity=mailfrom;\r\n\treceiver=; client-ip=\"\";\r\n\thelo=\"\";\r\n\tenvelope-from=\"\"\r\n".len();
    let helo = 2 * crate::format::row::MAX_EHLO;
    let sender = (2 * crate::format::row::MAX_ADDRESS).max("postmaster@".len() + helo);
    Ok(trace_allowance(hostname)? + frame + hostname.len() + 39 + helo + sender)
}

const DKIM_NAME_BYTES: usize = 253;
const DKIM_ROW_BYTES: usize =
    b"\r\n\tresult=permerror; a=ed25519-sha256; bits=65535; d=; s=".len() + 2 * DKIM_NAME_BYTES;
const DKIM_FIXED_BYTES: usize = b"X-TD-DKIM-Results: incomplete; receiver=".len()
    + crate::dkim::MAX_SIGNATURES * DKIM_ROW_BYTES
    + 7;

pub(crate) fn auth_trace_allowance(hostname: &str) -> Result<usize, Error> {
    Ok(spf_trace_allowance(hostname)? + DKIM_FIXED_BYTES + hostname.len())
}

#[derive(Clone, Copy, Default)]
pub(crate) struct Observations<'a> {
    pub spf: Option<crate::spf::Outcome>,
    pub dkim: Option<&'a crate::dkim::Report>,
}

impl Observations<'_> {
    pub fn allowance(self, hostname: &str) -> Result<usize, Error> {
        match (self.spf, self.dkim) {
            (None, None) => trace_allowance(hostname),
            (Some(_), None) => spf_trace_allowance(hostname),
            (Some(_), Some(_)) => auth_trace_allowance(hostname),
            (None, Some(_)) => Err(Error::Invalid),
        }
    }
}

fn valid_trace_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= DKIM_NAME_BYTES
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn dkim_trace(
    output: &mut String,
    hostname: &str,
    report: &crate::dkim::Report,
) -> Result<(), Error> {
    let start = output.len();
    let rows = || report.signatures.iter().take(crate::dkim::MAX_SIGNATURES);
    let invalid = rows().any(|row| {
        [row.domain.as_deref(), row.selector.as_deref()]
            .into_iter()
            .flatten()
            .any(|name| !valid_trace_name(name))
    });
    let status =
        if invalid || report.incomplete || report.signatures.len() > crate::dkim::MAX_SIGNATURES {
            "incomplete"
        } else if report.signatures.is_empty() {
            "none"
        } else {
            "complete"
        };
    write!(output, "X-TD-DKIM-Results: {status}; receiver={hostname}")
        .map_err(|_| Error::Capacity)?;
    if !invalid {
        for row in rows() {
            write!(output, "\r\n\tresult={}", row.outcome.as_str()).map_err(|_| Error::Capacity)?;
            if let Some(algorithm) = row.algorithm {
                write!(output, "; a={}", algorithm.as_str()).map_err(|_| Error::Capacity)?;
            }
            if let Some(bits) = row.key_bits {
                write!(output, "; bits={bits}").map_err(|_| Error::Capacity)?;
            }
            if let Some(domain) = &row.domain {
                write!(output, "; d={domain}").map_err(|_| Error::Capacity)?;
            }
            if let Some(selector) = &row.selector {
                write!(output, "; s={selector}").map_err(|_| Error::Capacity)?;
            }
        }
    }
    // Fill with folded comments to give every verdict and timeout the same
    // stored length. Each physical line contains text and stays below 998 bytes.
    let used = output.len() - start;
    let mut remaining = (DKIM_FIXED_BYTES + hostname.len() - 2)
        .checked_sub(used)
        .ok_or(Error::Capacity)?;
    while remaining > 0 {
        let fold = if remaining > 996 {
            (remaining - 5).min(996)
        } else {
            remaining
        };
        let spaces = fold.checked_sub(5).ok_or(Error::Invalid)?;
        write!(output, "\r\n\t({empty:spaces$})", empty = "").map_err(|_| Error::Capacity)?;
        remaining -= fold;
    }
    output.write_str("\r\n").map_err(|_| Error::Capacity)
}

struct QuotedContent<'a>(&'a str);
impl std::fmt::Display for QuotedContent<'_> {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0.bytes() {
            if !byte.is_ascii_graphic() && byte != b' ' {
                return Err(std::fmt::Error);
            }
            if matches!(byte, b'\\' | b'"') {
                output.write_char('\\')?;
            }
            output.write_char(char::from(byte))?;
        }
        Ok(())
    }
}
pub(crate) fn trace(
    receipt: &SmtpReceipt<'_>,
    hostname: &str,
    extended: bool,
    email: EmailId,
    utc_ms: i64,
    maximum: usize,
    observed: Observations<'_>,
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
    write!(output, "Return-Path: <{}>\r\n", receipt.reverse_path)
        .map_err(|_| ports::Error::Capacity)?;
    if observed.dkim.is_some() && observed.spf.is_none() {
        return Err(Error::Invalid);
    }
    if let Some(result) = observed.spf {
        if receipt.gateway.is_some() {
            return Err(ports::Error::Invalid);
        }
        let (prefix, mailbox) = if receipt.reverse_path.is_empty() {
            ("postmaster@", receipt.ehlo)
        } else {
            ("", receipt.reverse_path)
        };
        // Folding whitespace keeps the committed byte charge and header ceiling
        // independent of the authentication result.
        let width = "permerror".len().saturating_sub(result.as_str().len());
        write!(
            output,
            "Received-SPF: {} identity=mailfrom;\r\n\t{pad:width$}receiver={hostname}; client-ip=\"{}\";\r\n\thelo=\"{}\";\r\n\tenvelope-from=\"{prefix}{}\"\r\n",
            result.as_str(), receipt.peer, QuotedContent(receipt.ehlo), QuotedContent(mailbox), pad = ""
        )
        .map_err(|_| ports::Error::Invalid)?;
    }
    if let Some(report) = observed.dkim {
        dkim_trace(&mut output, hostname, report)?;
    }
    write!(output, "Received: from {} (", receipt.ehlo).map_err(|_| ports::Error::Capacity)?;
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod spf_tests {
    use super::*;
    use crate::{format::row::ReceiptRecipients, spf::Outcome};

    fn render(
        host: &str,
        ehlo: &str,
        reverse_path: &str,
        gateway: Option<&str>,
        observed: Option<Outcome>,
    ) -> Result<String, Error> {
        render_auth(
            host,
            ehlo,
            reverse_path,
            gateway,
            Observations {
                spf: observed,
                dkim: None,
            },
        )
    }
    fn render_auth(
        host: &str,
        ehlo: &str,
        reverse_path: &str,
        gateway: Option<&str>,
        observed: Observations<'_>,
    ) -> Result<String, Error> {
        let mut encoded = [0; 64];
        let receipt = SmtpReceipt {
            peer: "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff".parse().unwrap(),
            gateway,
            tls: ReceiptTls::Tls13,
            ehlo,
            reverse_path,
            recipients: ReceiptRecipients::encode(&["owner@example.test"], &mut encoded).unwrap(),
        };
        trace(
            &receipt,
            host,
            true,
            EmailId::from_bytes([1; 16]),
            0,
            observed.allowance(host)?,
            observed,
        )
    }

    #[test]
    fn dkim_report_uses_a_fixed_charge_and_a_trusted_position() {
        use crate::dkim::{Outcome as DkimOutcome, Report, SignatureResult, MAX_SIGNATURES};
        let host = "mx.example.test";
        let unavailable = Report::unavailable();
        let render = |report: &Report| {
            render_auth(
                host,
                "sender.test",
                "",
                None,
                Observations {
                    spf: Some(Outcome::Pass),
                    dkim: Some(report),
                },
            )
        };
        let field = |trace: &str| {
            trace
                .split_once("\r\nX-TD-DKIM-Results: ")
                .unwrap()
                .1
                .split_once("\r\nReceived: ")
                .unwrap()
                .0
                .to_owned()
        };
        let none = Report {
            signatures: Vec::new(),
            incomplete: false,
        };
        let unsigned = render(&none).unwrap();
        assert!(field(&unsigned).starts_with("none; receiver=mx.example.test\r\n"));
        assert!(field(&render(&unavailable).unwrap()).starts_with("incomplete; receiver="));
        assert_eq!(render(&unavailable).unwrap().len(), unsigned.len());
        assert_eq!(
            auth_trace_allowance(host).unwrap() - spf_trace_allowance(host).unwrap(),
            2299 + host.len()
        );
        assert!(
            unsigned.find("Received-SPF:").unwrap() < unsigned.find("X-TD-DKIM-Results:").unwrap()
        );

        let longest = [
            "x".repeat(63),
            "y".repeat(63),
            "z".repeat(63),
            "a".repeat(61),
        ]
        .join(".");
        for outcome in [
            DkimOutcome::Pass,
            DkimOutcome::Fail,
            DkimOutcome::Temperror,
            DkimOutcome::Permerror,
        ] {
            let details = || SignatureResult {
                outcome,
                algorithm: Some(td_crypto::DkimAlgorithm::Ed25519),
                key_bits: Some(256),
                domain: Some(longest.clone()),
                selector: Some(longest.clone()),
            };
            let full = Report {
                signatures: (0..MAX_SIGNATURES).map(|_| details()).collect(),
                incomplete: true,
            };
            let raw = render(&full).unwrap();
            assert_eq!(raw.len(), unsigned.len());
            assert!(raw.split("\r\n").all(|line| line.len() <= 998));
            assert_eq!(
                field(&raw)
                    .matches(&format!(
                        "result={}; a=ed25519-sha256; bits=256; d={longest}; s={longest}",
                        outcome.as_str()
                    ))
                    .count(),
                MAX_SIGNATURES
            );
        }

        let invalid = Report {
            signatures: vec![SignatureResult {
                outcome: DkimOutcome::Pass,
                algorithm: Some(td_crypto::DkimAlgorithm::Rsa),
                key_bits: Some(1024),
                domain: Some("x\r\nForged: pass".into()),
                selector: None,
            }],
            incomplete: false,
        };
        assert_eq!(render(&invalid).unwrap(), render(&unavailable).unwrap());
        assert_eq!(
            render_auth(
                host,
                "sender.test",
                "",
                Some("gateway"),
                Observations {
                    spf: Some(Outcome::Pass),
                    dkim: Some(&none)
                }
            ),
            Err(Error::Invalid)
        );
        assert_eq!(
            render_auth(
                host,
                "sender.test",
                "",
                None,
                Observations {
                    spf: None,
                    dkim: Some(&none)
                }
            ),
            Err(Error::Invalid)
        );
    }

    const OUTCOMES: &[Outcome] = &[
        Outcome::None,
        Outcome::Neutral,
        Outcome::Pass,
        Outcome::Fail,
        Outcome::Softfail,
        Outcome::Temperror,
        Outcome::Permerror,
    ];

    #[test]
    fn received_spf_precedes_the_receivers_own_received_field() {
        const HOST: &str = "mx.example.test";
        const HELO: &str = "[IPv6:::1]";
        let size = render(HOST, HELO, "", None, Some(Outcome::Permerror))
            .unwrap()
            .len();
        for &outcome in OUTCOMES {
            let header = render(HOST, HELO, "", None, Some(outcome)).unwrap();
            assert_eq!(header.len(), size);
            assert!(header.starts_with(&format!(
                "Return-Path: <>\r\nReceived-SPF: {} identity=mailfrom;\r\n\t{}receiver=mx.example.test; client-ip=\"ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff\";\r\n\thelo=\"[IPv6:::1]\";\r\n\tenvelope-from=\"postmaster@[IPv6:::1]\"\r\nReceived: ",
                outcome.as_str(), " ".repeat("permerror".len() - outcome.as_str().len())
            )));
        }
        for gateway in [None, Some("gateway")] {
            let header = render(HOST, HELO, "", gateway, None).unwrap();
            assert!(header.starts_with("Return-Path: <>\r\nReceived: "));
        }
        assert_eq!(
            render(HOST, HELO, "", Some("gateway"), Some(Outcome::Pass)),
            Err(Error::Invalid)
        );
        let quoted = render(
            HOST,
            HELO,
            r#""a\"b"@example.test"#,
            None,
            Some(Outcome::None),
        )
        .unwrap();
        assert!(quoted.contains(r#"envelope-from="\"a\\\"b\"@example.test""#));
        assert_eq!(
            render(HOST, "a\r\nForged: pass", "", None, Some(Outcome::None)),
            Err(Error::Invalid)
        );
    }

    #[test]
    fn local_verdict_and_sender_trace_fit_the_exact_filtered_header_budget() {
        let clock = crate::clock::RuntimeClock::new();
        let mut last = clock.monotonic().unwrap();
        let deadline = Deadline::after(last, 10_000).unwrap();
        let mut meter = Meter::new(
            deadline,
            td_mime::work::Charge {
                records: 1_000_000,
                io_bytes: 1_000_000,
                output_bytes: 1_000_000,
                unlinks: 0,
            },
        );
        let mut work = Work {
            clock: &clock,
            deadline,
            meter: &mut meter,
            last: &mut last,
        };
        let original = b"Received-SPF: pass (prior hop)\r\n";
        let tail = b"\r\nbody\r\n";
        let source = [
            b"Return-Path: <injected@example.test>\r\n".as_slice(),
            original,
            tail,
        ]
        .concat();
        let trace = |outcome| {
            render(
                "mx.example.test",
                "mail.example.test",
                "user@example.test",
                None,
                Some(outcome),
            )
            .unwrap()
        };
        let limit = trace(Outcome::Fail).len() + original.len();
        let filtered = |trace: &str, limit, work: &mut Work<'_>| -> Result<Vec<u8>, HeaderError> {
            let mut headers = Headers::resident(&source, limit);
            assert!(headers.push(&source, true, work)?);
            let mut raw = Vec::new();
            headers.write_filtered(
                trace.as_bytes(),
                |bytes| {
                    raw.extend_from_slice(bytes);
                    Ok(())
                },
                work,
            )?;
            Ok(raw)
        };
        for &outcome in OUTCOMES {
            let trace = trace(outcome);
            assert_eq!(
                filtered(&trace, limit - 1, &mut work),
                Err(HeaderError::HeaderLimit)
            );
            let raw = filtered(&trace, limit, &mut work).unwrap();
            assert_eq!(raw, [trace.as_bytes(), original, tail].concat());
            // PostgreSQL charges this exact byte length to the account.
            assert_eq!(raw.len(), limit + tail.len());
        }
    }

    #[test]
    fn maximum_quoted_properties_fit_the_advertised_trace_allowance() {
        let host = [
            "x".repeat(63),
            "y".repeat(63),
            "z".repeat(63),
            "t".repeat(61),
        ]
        .join(".");
        let helo = "\\".repeat(crate::format::row::MAX_EHLO);
        let sender = "\"".repeat(crate::format::row::MAX_ADDRESS);
        for reverse in ["", sender.as_str()] {
            let header = render(&host, &helo, reverse, None, Some(Outcome::Temperror)).unwrap();
            assert!(header.len() + 2 <= spf_trace_allowance(&host).unwrap());
            assert!(header.split("\r\n").all(|line| line.len() <= 998));
        }
    }
}
