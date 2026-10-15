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
    let provenance = Provenance {
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
    };
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
    let mut bytes = Vec::new();
    let mut append = |part: &[u8]| {
        if bytes
            .len()
            .checked_add(part.len())
            .is_none_or(|size| size > maximum)
        {
            return Err(crate::ports::Error::Capacity);
        }
        bytes
            .try_reserve(part.len())
            .map_err(|_| crate::ports::Error::Capacity)?;
        bytes.extend_from_slice(part);
        Ok(())
    };
    let mut remaining = message.bytes;
    while !remaining.is_empty() {
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
        if header.complete() {
            let now = work.tick().map_err(Error::Input)?;
            work.meter
                .charge(
                    now,
                    td_mime::work::Charge {
                        io_bytes: line.len() as u64,
                        output_bytes: line.len() as u64,
                        records: 0,
                        unlinks: 0,
                    },
                )
                .map_err(|error| {
                    Error::Input(if error == td_mime::work::Stop::Deadline {
                        crate::ports::Error::Deadline
                    } else {
                        crate::ports::Error::Capacity
                    })
                })?;
            append(line).map_err(Error::Input)?;
        } else if header.push(line, false, &mut work).map_err(header_error)? {
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
    let mut anchors = Vec::new();
    anchors.try_reserve_exact(65).map_err(|_| Error::Capacity)?;
    let mut scratch = [0; crate::format::key::MAX_ANCHOR_BYTES];
    for ring in [&header.identifiers.references, &header.identifiers.reply] {
        for index in 0..32 {
            let Some(item) = ring.newest(index) else {
                break;
            };
            if let Some(id) = headers::candidate(&header.source, item, &mut scratch, &mut work)
                .map_err(Error::Input)?
            {
                charge(clock, work.meter, id.len(), id.len())?;
                anchors.push(copy(id)?);
            }
        }
    }
    let own = header
        .identifiers
        .own
        .map(|item| {
            headers::candidate(&header.source, item, &mut scratch, &mut work)
                .map_err(Error::Input)?
                .map(|id| {
                    charge(clock, work.meter, id.len(), id.len())?;
                    copy(id)
                })
                .transpose()
        })
        .transpose()?
        .flatten()
        .unwrap_or_default();
    if !own.is_empty() {
        charge(clock, work.meter, own.len(), own.len())?;
        anchors.push(copy(&own)?);
    }
    charge(clock, work.meter, 0, 0)?;
    Ok((bytes, anchors, own, provenance))
}
fn header_error(error: headers::HeaderError) -> Error {
    match error {
        headers::HeaderError::HeaderLimit => Error::HeaderLimit,
        headers::HeaderError::Work(error) => Error::Input(error),
    }
}
