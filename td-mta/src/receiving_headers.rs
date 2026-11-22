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
    /// Drops supplied Return-Path fields and Authentication-Results fields
    /// that claim `authserv_id` or whose authserv-id cannot be read.
    pub fn write_filtered(
        &self,
        trace: &[u8],
        authserv_id: &str,
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
                    let forged = name.eq_ignore_ascii_case(b"authentication-results")
                        && crate::sender_authentication::claims_receiver(
                            extent(&self.source, field.value_start, field.value_end)?,
                            authserv_id,
                        );
                    if !name.eq_ignore_ascii_case(b"return-path") && !forged {
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
/// The direct-MX trace adds one fixed-length Authentication-Results field.
pub(crate) fn auth_trace_allowance(hostname: &str) -> Result<usize, ports::Error> {
    trace_allowance(hostname)?
        .checked_add(crate::sender_authentication::field_bytes(hostname)?)
        .ok_or(ports::Error::Capacity)
}

pub(crate) fn allowance(
    hostname: &str,
    results: Option<&crate::sender_authentication::Results>,
) -> Result<usize, ports::Error> {
    match results {
        None => trace_allowance(hostname),
        Some(_) => auth_trace_allowance(hostname),
    }
}

pub(crate) fn trace(
    receipt: &SmtpReceipt<'_>,
    hostname: &str,
    extended: bool,
    email: EmailId,
    utc_ms: i64,
    maximum: usize,
    results: Option<&crate::sender_authentication::Results>,
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
    if let Some(results) = results {
        if receipt.gateway.is_some() {
            return Err(ports::Error::Invalid);
        }
        results.write_field(&mut output, hostname)?;
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
mod authentication_tests {
    use super::*;
    use crate::{
        dkim, dmarc,
        format::row::ReceiptRecipients,
        sender_authentication::{Observed, Results},
        spf::Outcome,
    };

    const HOST: &str = "mx.example.test";

    fn longest() -> String {
        [
            "x".repeat(63),
            "y".repeat(63),
            "z".repeat(63),
            "a".repeat(61),
        ]
        .join(".")
    }

    fn results(spf: Outcome, dkim: dkim::Report, dmarc: dmarc::Report, reverse: &str) -> Results {
        Results::new(Observed { spf, dkim, dmarc }, "sender.test", reverse).unwrap()
    }

    fn render(
        host: &str,
        reverse_path: &str,
        gateway: Option<&str>,
        results: Option<&Results>,
    ) -> Result<String, Error> {
        let mut encoded = [0; 64];
        let receipt = SmtpReceipt {
            peer: "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff".parse().unwrap(),
            gateway,
            tls: ReceiptTls::Tls13,
            ehlo: "sender.test",
            reverse_path,
            recipients: ReceiptRecipients::encode(&["owner@example.test"], &mut encoded).unwrap(),
        };
        trace(
            &receipt,
            host,
            true,
            EmailId::from_bytes([1; 16]),
            0,
            allowance(host, results)?,
            results,
        )
    }

    fn field(trace: &str) -> &str {
        trace
            .split_once("\r\nAuthentication-Results: ")
            .unwrap()
            .1
            .split_once("\r\nReceived: ")
            .unwrap()
            .0
    }

    fn unsigned() -> dkim::Report {
        dkim::Report {
            signatures: Vec::new(),
            incomplete: false,
        }
    }

    #[test]
    fn authentication_results_sits_between_return_path_and_received() {
        let none = results(Outcome::None, unsigned(), dmarc::Report::unavailable(), "");
        let trace = render(HOST, "", None, Some(&none)).unwrap();
        assert!(trace.starts_with(
            "Return-Path: <>\r\nAuthentication-Results: mx.example.test;\r\n\tspf=none smtp.helo=sender.test;\r\n\tdkim=none;\r\n\tdmarc=temperror\r\n\t("
        ), "{trace}");
        assert!(trace.contains(")\r\nReceived: from sender.test ([IPv6:"));
        assert_eq!(
            auth_trace_allowance(HOST).unwrap() - trace_allowance(HOST).unwrap(),
            3297 + HOST.len()
        );
        for gateway in [None, Some("gateway")] {
            let plain = render(HOST, "", gateway, None).unwrap();
            assert!(plain.starts_with("Return-Path: <>\r\nReceived: "));
        }
        assert_eq!(
            render(HOST, "", Some("gateway"), Some(&none)),
            Err(Error::Invalid)
        );
    }

    #[test]
    fn every_verdict_renders_the_same_field_length() {
        let reference = render(
            HOST,
            "",
            None,
            Some(&results(
                Outcome::None,
                dkim::Report::unavailable(),
                dmarc::Report::unavailable(),
                "",
            )),
        )
        .unwrap();
        let longest = longest();
        let sender = format!("owner@{longest}");
        let dkim_outcomes = [
            dkim::Outcome::Pass,
            dkim::Outcome::Fail,
            dkim::Outcome::Temperror,
            dkim::Outcome::Permerror,
        ];
        let dmarc_outcomes = [
            dmarc::Outcome::None,
            dmarc::Outcome::Pass,
            dmarc::Outcome::Fail,
            dmarc::Outcome::Temperror,
            dmarc::Outcome::Permerror,
        ];
        for (index, spf) in [
            Outcome::None,
            Outcome::Neutral,
            Outcome::Pass,
            Outcome::Fail,
            Outcome::Softfail,
            Outcome::Temperror,
            Outcome::Permerror,
        ]
        .into_iter()
        .enumerate()
        {
            let row = |outcome| dkim::SignatureResult {
                outcome,
                algorithm: Some(td_crypto::DkimAlgorithm::Ed25519),
                key_bits: Some(65535),
                domain: Some(longest.clone()),
                selector: Some(longest.clone()),
            };
            let outcome = *dkim_outcomes.get(index % dkim_outcomes.len()).unwrap();
            let full = dkim::Report {
                signatures: (0..dkim::MAX_SIGNATURES).map(|_| row(outcome)).collect(),
                incomplete: true,
            };
            let dmarc = dmarc::Report {
                outcome: *dmarc_outcomes.get(index % dmarc_outcomes.len()).unwrap(),
                author: Some(longest.clone()),
                policy_domain: Some(longest.clone()),
                policy: Some(dmarc::Requested {
                    disposition: dmarc::Disposition::Quarantine,
                    strict_spf: true,
                    strict_dkim: true,
                }),
                aligned: Some(dmarc::Identifier::Dkim),
            };
            let trace = render(
                HOST,
                &sender,
                None,
                Some(&results(spf, full, dmarc, &sender)),
            )
            .unwrap();
            assert!(trace.split("\r\n").all(|line| line.len() <= 998));
            let field = field(&trace);
            assert_eq!(field.len(), self::field(&reference).len());
            assert!(field.starts_with(&format!(
                "mx.example.test;\r\n\tspf={} smtp.mailfrom={longest};",
                spf.as_str()
            )));
            assert_eq!(
                field
                    .matches(&format!(
                        "\r\n\tdkim={} (65535-bit key) header.d={longest} header.s={longest} header.a=ed25519-sha256;",
                        outcome.as_str()
                    ))
                    .count(),
                dkim::MAX_SIGNATURES
            );
            assert!(field.contains(";\r\n\tdkim=temperror (verification incomplete);"));
            assert!(field.contains(&format!(
                " (policy=quarantine aspf=s adkim=s aligned=dkim policy-domain={longest}) header.from={longest}\r\n\t("
            )));
        }
    }

    #[test]
    fn unrepresentable_names_withdraw_the_observation() {
        let forged = dkim::Report {
            signatures: vec![dkim::SignatureResult {
                outcome: dkim::Outcome::Pass,
                algorithm: Some(td_crypto::DkimAlgorithm::Rsa),
                key_bits: Some(1024),
                domain: Some("x\r\nForged: pass".into()),
                selector: None,
            }],
            incomplete: false,
        };
        let author = dmarc::Report {
            outcome: dmarc::Outcome::Pass,
            author: Some("x; dkim=pass".into()),
            policy_domain: None,
            policy: None,
            aligned: Some(dmarc::Identifier::Spf),
        };
        let withdrawn = results(Outcome::Pass, forged, author, "a@sender.test");
        assert_eq!(withdrawn.dkim, dkim::Report::unavailable());
        assert_eq!(withdrawn.dmarc, dmarc::Report::unavailable());
        let trace = render(HOST, "a@sender.test", None, Some(&withdrawn)).unwrap();
        assert!(field(&trace).starts_with(
            "mx.example.test;\r\n\tspf=pass smtp.mailfrom=sender.test;\r\n\tdkim=temperror (verification incomplete);\r\n\tdmarc=temperror\r\n"
        ));
    }

    #[test]
    fn dmarc_resting_on_withdrawn_dkim_is_withdrawn_with_it() {
        let forged = || dkim::Report {
            signatures: vec![dkim::SignatureResult {
                outcome: dkim::Outcome::Pass,
                algorithm: Some(td_crypto::DkimAlgorithm::Rsa),
                key_bits: Some(2048),
                domain: Some("bad domain".into()),
                selector: None,
            }],
            incomplete: false,
        };
        let aligned = |identifier| dmarc::Report {
            outcome: dmarc::Outcome::Pass,
            author: Some("sender.test".into()),
            policy_domain: Some("sender.test".into()),
            policy: Some(dmarc::Requested {
                disposition: dmarc::Disposition::Reject,
                strict_spf: false,
                strict_dkim: false,
            }),
            aligned: Some(identifier),
        };
        let dkim = results(
            Outcome::Pass,
            forged(),
            aligned(dmarc::Identifier::Dkim),
            "a@sender.test",
        );
        assert_eq!(dkim.dmarc, dmarc::Report::unavailable());
        let spf = results(
            Outcome::Pass,
            forged(),
            aligned(dmarc::Identifier::Spf),
            "a@sender.test",
        );
        assert_eq!(spf.dkim, dkim::Report::unavailable());
        assert_eq!(spf.dmarc, aligned(dmarc::Identifier::Spf));
        assert!(!spf.spf_helo);
        let trace = render(HOST, "a@sender.test", None, Some(&spf)).unwrap();
        assert!(field(&trace).contains(
            ";\r\n\tdmarc=pass (policy=reject aspf=r adkim=r aligned=spf policy-domain=sender.test) header.from=sender.test\r\n"
        ), "{trace}");
    }

    fn filter(source: &[u8], trace: &str, limit: usize) -> Result<Vec<u8>, HeaderError> {
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
        let mut headers = Headers::resident(source, limit);
        assert!(headers.push(source, true, &mut work)?);
        let mut raw = Vec::new();
        headers.write_filtered(
            trace.as_bytes(),
            HOST,
            |bytes| {
                raw.extend_from_slice(bytes);
                Ok(())
            },
            &mut work,
        )?;
        Ok(raw)
    }

    #[test]
    fn supplied_fields_claiming_this_receiver_are_removed() {
        let kept: &[&[u8]] = &[
            b"Authentication-Results: other.example.test; spf=pass\r\n",
            b"Authentication-Results: mx.example.test.evil; dkim=pass\r\n",
            b"Authentication-Results: \"mx.example.test.\"; none\r\n",
            b"Received-SPF: pass (prior hop)\r\n",
        ];
        let removed: &[&[u8]] = &[
            b"Return-Path: <injected@example.test>\r\n",
            b"Authentication-Results: mx.example.test; spf=pass\r\n",
            b"authentication-results:MX.Example.Test;dkim=pass\r\n",
            b"Authentication-Results: (comment (nested)) \"mx.ex\\ample.test\" 1;\r\n\tdmarc=pass\r\n",
            b"Authentication-Results: ; none\r\n",
            b"Authentication-Results: (unterminated\r\n",
            b"Authentication-Results: \"unterminated\r\n",
        ];
        let tail = b"\r\nbody\r\n";
        let source = [kept.concat(), removed.concat(), tail.to_vec()].concat();
        // The scanner's ceiling also bounds the supplied header section.
        let trace = format!("Return-Path: <>\r\nX-Pad: {}\r\n", "x".repeat(512));
        let limit = trace.len() + kept.concat().len();
        let raw = filter(&source, &trace, limit).unwrap();
        assert_eq!(raw, [trace.as_bytes(), &kept.concat(), tail].concat());
        assert_eq!(
            filter(&source, &trace, limit - 1),
            Err(HeaderError::HeaderLimit)
        );
    }

    #[test]
    fn maximum_envelope_values_fit_the_advertised_trace_allowance() {
        let host = longest();
        let sender = "\"".repeat(crate::format::row::MAX_ADDRESS);
        for reverse in ["", sender.as_str()] {
            let observed = results(
                Outcome::Temperror,
                dkim::Report::unavailable(),
                dmarc::Report::unavailable(),
                reverse,
            );
            let header = render(&host, reverse, None, Some(&observed)).unwrap();
            assert!(header.len() + 2 <= auth_trace_allowance(&host).unwrap());
            assert!(header.split("\r\n").all(|line| line.len() <= 998));
        }
    }
}
