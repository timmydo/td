//! Trusted receiving facts and bounded DATA preparation before publication.
use super::*;
use crate::{
    admission::work::Meter,
    format::row::{EmailOrigin, EmailRow, ReceiptTls, Row, SmtpReceipt},
    ports::Clock,
    receiving_headers as headers,
};
use std::net::IpAddr;

/// Supplied by the trusted SMTP connection/policy owner after DATA completes.
/// These facts are not credentials or a replacement for gateway admission.
pub struct Message<'a> {
    pub bytes: &'a [u8],
    pub receipt: SmtpReceipt<'a>,
    pub hostname: &'a str,
    pub extended: bool,
    pub header_bytes: usize,
    pub received_at: i64,
}

#[derive(Clone)]
pub(super) struct Provenance {
    pub peer: IpAddr,
    pub tls: i64,
    pub gateway: Option<String>,
    pub ehlo: String,
    pub reverse: String,
    pub recipients: Vec<String>,
    pub encoded: Vec<u8>,
    pub received_at: i64,
}

pub(super) fn prepare(
    binding: &Binding,
    message: &Message<'_>,
    email: EmailId,
    blob: BlobId,
    thread: ThreadId,
    clock: &impl Clock,
    meter: &mut Meter,
) -> Result<(Vec<u8>, Vec<String>, String, Provenance), Error> {
    let maximum = binding.upload_maximum().min(32 * 1024 * 1024);
    let allowance = headers::trace_allowance(message.hostname).map_err(Error::Input)?;
    if message.header_bytes == 0
        || message.header_bytes > 1024 * 1024
        || message.header_bytes > maximum
        || message.bytes.len() > maximum.checked_sub(allowance).ok_or(Error::Capacity)?
    {
        return Err(Error::Capacity);
    }
    // Prepay bounded metadata validation, encoding, copies and trace scratch.
    charge(
        clock,
        meter,
        4 * crate::format::MAX_VALUE_BYTES,
        2 * crate::format::MAX_VALUE_BYTES + allowance,
    )?;
    let provenance = provenance(message, blob, thread)?;
    let trace = headers::trace(
        &message.receipt,
        message.hostname,
        message.extended,
        email,
        message.received_at,
        allowance,
    )
    .map_err(Error::Input)?;
    let mut last = clock.sample().map_err(Error::Input)?.monotonic;
    let cap = crate::ports::Deadline::after(crate::ports::Tick(0), meter.deadline().tick().0)
        .map_err(Error::Input)?;
    let mut work = headers::Work {
        clock,
        deadline: cap,
        meter,
        last: &mut last,
    };
    let mut header = headers::Headers::new(message.header_bytes).map_err(header_error)?;
    let output_limit = message
        .bytes
        .len()
        .checked_add(allowance)
        .filter(|size| *size <= maximum)
        .ok_or(Error::Capacity)?;
    let mut bytes = Vec::new();
    // Filtering only removes input; allowance includes trace and separator.
    bytes
        .try_reserve_exact(output_limit)
        .map_err(|_| Error::Capacity)?;
    let mut append = |part: &[u8]| {
        if bytes
            .len()
            .checked_add(part.len())
            .is_none_or(|size| size > output_limit)
        {
            return Err(crate::ports::Error::Capacity);
        }
        bytes.extend_from_slice(part);
        Ok(())
    };
    let mut remaining = message.bytes;
    while !remaining.is_empty() && !header.complete() {
        let window = remaining
            .get(..remaining.len().min(1001))
            .ok_or(Error::Integrity)?;
        charge(clock, work.meter, window.len(), 0)?;
        let length = window
            .iter()
            .position(|byte| *byte == b'\n')
            .and_then(|n| n.checked_add(1))
            .ok_or(Error::Input(crate::ports::Error::Invalid))?;
        let line = remaining.get(..length).ok_or(Error::Integrity)?;
        remaining = remaining.get(length..).ok_or(Error::Integrity)?;
        if line.len() > 1000 || !line.ends_with(b"\r\n") {
            return Err(Error::Input(crate::ports::Error::Invalid));
        }
        if header.push(line, false, &mut work).map_err(header_error)? {
            header
                .write_filtered(trace.as_bytes(), &mut append, &mut work)
                .map_err(header_error)?;
        }
    }
    if !header.complete() {
        if !header.push(&[], true, &mut work).map_err(header_error)? {
            return Err(Error::Integrity);
        }
        header
            .write_filtered(trace.as_bytes(), &mut append, &mut work)
            .map_err(header_error)?;
    }
    copy_body(remaining, &mut append, &mut work)?;
    let (anchors, own) = header.thread_identifiers(&mut work).map_err(Error::Input)?;
    charge(clock, work.meter, 0, 0)?;
    Ok((bytes, anchors, own, provenance))
}
// Preserve per-line work charges, but sample deadlines once per bounded turn.
fn copy_body(
    mut remaining: &[u8],
    append: &mut impl FnMut(&[u8]) -> Result<(), crate::ports::Error>,
    work: &mut headers::Work<'_>,
) -> Result<(), Error> {
    const TURN_BYTES: usize = 64 * 1024;
    while !remaining.is_empty() {
        let now = work.tick().map_err(Error::Input)?;
        let mut consumed = 0;
        while !remaining.is_empty() && consumed < TURN_BYTES {
            let window = remaining
                .get(..remaining.len().min(1001))
                .ok_or(Error::Integrity)?;
            work.meter
                .charge(
                    now,
                    td_mime::work::Charge {
                        io_bytes: window.len() as u64,
                        records: 1,
                        ..td_mime::work::Charge::default()
                    },
                )
                .map_err(body_work_error)?;
            let length = window
                .iter()
                .position(|byte| *byte == b'\n')
                .and_then(|n| n.checked_add(1))
                .ok_or(Error::Input(crate::ports::Error::Invalid))?;
            let line = remaining.get(..length).ok_or(Error::Integrity)?;
            if line.len() > 1000 || !line.ends_with(b"\r\n") {
                return Err(Error::Input(crate::ports::Error::Invalid));
            }
            work.meter
                .charge(
                    now,
                    td_mime::work::Charge {
                        io_bytes: line.len() as u64,
                        output_bytes: line.len() as u64,
                        ..td_mime::work::Charge::default()
                    },
                )
                .map_err(body_work_error)?;
            append(line).map_err(Error::Input)?;
            remaining = remaining.get(length..).ok_or(Error::Integrity)?;
            consumed += length;
        }
    }
    work.tick().map_err(Error::Input)?;
    Ok(())
}
fn body_work_error(error: td_mime::work::Stop) -> Error {
    Error::Input(if error == td_mime::work::Stop::Deadline {
        crate::ports::Error::Deadline
    } else {
        crate::ports::Error::Capacity
    })
}

fn header_error(error: headers::HeaderError) -> Error {
    match error {
        headers::HeaderError::HeaderLimit => Error::HeaderLimit,
        headers::HeaderError::Work(error) => Error::Input(error),
    }
}

pub(super) fn provenance(
    message: &Message<'_>,
    blob: BlobId,
    thread: ThreadId,
) -> Result<Provenance, Error> {
    let row = Row::Email(EmailRow {
        blob,
        thread,
        received_at: message.received_at,
        origin: EmailOrigin::Smtp(message.receipt),
    });
    let size = row.encoded_len().map_err(|_| Error::Integrity)?;
    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(size)
        .map_err(|_| Error::Capacity)?;
    encoded.resize(size, 0);
    row.encode(&mut encoded).map_err(|_| Error::Integrity)?;
    let mut recipients = Vec::new();
    for address in message
        .receipt
        .recipients
        .iter()
        .map_err(|_| Error::Integrity)?
    {
        recipients.try_reserve(1).map_err(|_| Error::Capacity)?;
        recipients.push(copy(address.map_err(|_| Error::Integrity)?)?);
    }
    Ok(Provenance {
        peer: message.receipt.peer,
        tls: match message.receipt.tls {
            ReceiptTls::Plain => 0,
            ReceiptTls::Tls12 => 1,
            ReceiptTls::Tls13 => 2,
        },
        gateway: message.receipt.gateway.map(copy).transpose()?,
        ehlo: copy(message.receipt.ehlo)?,
        reverse: copy(message.receipt.reverse_path)?,
        recipients,
        encoded,
        received_at: message.received_at,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::ports::{Deadline, Tick, Time};
    use std::sync::atomic::{AtomicU64, Ordering};
    use td_mime::work::Charge;

    struct CountingClock(AtomicU64);
    impl Clock for CountingClock {
        fn sample(&self) -> Result<Time, crate::ports::Error> {
            Ok(Time {
                utc_ms: 0,
                monotonic: Tick(self.0.fetch_add(1, Ordering::Relaxed) + 1),
            })
        }
    }
    fn capacity() -> Charge {
        Charge {
            io_bytes: 256 * 1024 * 1024,
            output_bytes: 4 * 1024 * 1024,
            records: 1024 * 1024,
            unlinks: 0,
        }
    }
    fn run(
        body: &[u8],
        clock: &CountingClock,
        deadline: u64,
        capacity: Charge,
    ) -> (Result<(), Error>, Vec<u8>, Meter) {
        let deadline = Deadline::after(Tick(0), deadline).unwrap();
        let mut meter = Meter::new(deadline, capacity);
        let mut last = Tick(0);
        let mut output = Vec::new();
        let result = copy_body(
            body,
            &mut |part| {
                output.extend_from_slice(part);
                Ok(())
            },
            &mut headers::Work {
                clock,
                deadline,
                meter: &mut meter,
                last: &mut last,
            },
        );
        (result, output, meter)
    }
    #[test]
    fn body_turns_preserve_bytes_and_original_line_charges() {
        let body = b"short line\r\n".repeat(100_000);
        let clock = CountingClock(AtomicU64::new(0));
        let (result, output, meter) = run(&body, &clock, 100, capacity());
        assert_eq!(result, Ok(()));
        assert_eq!(output, body);
        let mut remaining = body.len();
        let scan_bytes: usize = body
            .split_inclusive(|b| *b == b'\n')
            .map(|line| {
                let scanned = remaining.min(1001);
                remaining -= line.len();
                scanned
            })
            .sum();
        assert_eq!(
            meter.remaining().io_bytes,
            capacity().io_bytes - (scan_bytes + body.len()) as u64
        );
        assert_eq!(
            meter.remaining().output_bytes,
            capacity().output_bytes - body.len() as u64
        );
        assert_eq!(meter.remaining().records, capacity().records - 100_000);
        assert!(clock.0.load(Ordering::Relaxed) < 32);
    }
    #[test]
    fn body_turns_keep_line_and_crlf_validation() {
        for body in [
            b"bare\n".to_vec(),
            b"unfinished\r".to_vec(),
            [vec![b'x'; 999], b"\r\n".to_vec()].concat(),
        ] {
            assert_eq!(
                run(&body, &CountingClock(AtomicU64::new(0)), 100, capacity()).0,
                Err(Error::Input(crate::ports::Error::Invalid))
            );
        }
        let line = [vec![b'x'; 998], b"\r\n".to_vec()].concat();
        let mut body = line.repeat(70);
        let (result, output, _) = run(&body, &CountingClock(AtomicU64::new(0)), 100, capacity());
        assert_eq!(result, Ok(()));
        assert_eq!(output, body);
        body.extend_from_slice(b"bare\n");
        assert_eq!(
            run(&body, &CountingClock(AtomicU64::new(0)), 100, capacity()).0,
            Err(Error::Input(crate::ports::Error::Invalid))
        );
    }
    #[test]
    fn body_turns_precharge_and_check_deadline_before_next_turn_and_return() {
        for exhausted in [
            Charge {
                io_bytes: 0,
                ..capacity()
            },
            Charge {
                output_bytes: 0,
                ..capacity()
            },
            Charge {
                records: 0,
                ..capacity()
            },
        ] {
            let (result, output, meter) = run(
                b"body\r\n",
                &CountingClock(AtomicU64::new(0)),
                100,
                exhausted,
            );
            assert_eq!(result, Err(Error::Input(crate::ports::Error::Capacity)));
            assert!(output.is_empty());
            assert!(meter.stopped().is_some());
        }
        let body = b"body\r\n".repeat(30_000);
        let (result, output, _) = run(&body, &CountingClock(AtomicU64::new(0)), 2, capacity());
        assert_eq!(result, Err(Error::Input(crate::ports::Error::Deadline)));
        assert!((65536..66536).contains(&output.len()));
        assert_eq!(
            run(
                b"body\r\n",
                &CountingClock(AtomicU64::new(0)),
                2,
                capacity()
            )
            .0,
            Err(Error::Input(crate::ports::Error::Deadline))
        );
    }
}
