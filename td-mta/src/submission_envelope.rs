//! Derive td-mail's implicit envelope from bounded immutable root headers.
use crate::{
    admission::work::{Charge, Meter, Stop},
    config::values,
    format::row::MAX_ADDRESS,
    mime_headers::{Scanner, Status},
    ports::{self, Clock, Deadline, Tick},
    transmitted_headers,
};
use td_mime::header_addresses::{self, Address};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Field {
    From,
    Sender,
    To,
    Cc,
    Bcc,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Policy(ports::Error),
    Headers,
    InvalidEmail(Field),
    /// Either author header names an address outside the selected identity.
    ForbiddenFrom,
    NoRecipients,
    TooManyRecipients,
}
impl From<ports::Error> for Error {
    fn from(error: ports::Error) -> Self {
        Self::Policy(error)
    }
}
struct AddressKey<'a> {
    address: &'a str,
    bytes: [u8; MAX_ADDRESS],
    length: usize,
}
impl AddressKey<'_> {
    fn key(&self) -> Result<&[u8], ports::Error> {
        self.bytes.get(..self.length).ok_or(ports::Error::Corrupt)
    }
}
/// Passive header evidence, not native custody, device or identity authority.
/// The consumer retains the same immutable source and selected configuration,
/// reserves queue capacity and reauthorizes before publishing responsibility.
pub struct Envelope<'a> {
    sender: &'a str,
    recipients: Vec<AddressKey<'a>>,
}
impl Envelope<'_> {
    pub fn mail_from(&self) -> &str {
        self.sender
    }
    pub fn recipients(&self) -> impl ExactSizeIterator<Item = &str> {
        self.recipients.iter().map(|entry| entry.address)
    }
}
impl std::fmt::Debug for Envelope<'_> {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("SubmissionEnvelope(<redacted>)")
    }
}
struct Work<'a> {
    clock: &'a dyn Clock,
    deadline: Deadline,
    last: Tick,
    meter: &'a mut Meter,
}
fn work(stop: Stop) -> ports::Error {
    if stop == Stop::Deadline {
        ports::Error::Deadline
    } else {
        ports::Error::Capacity
    }
}
impl Work<'_> {
    fn check(&mut self, charge: Charge) -> Result<Tick, ports::Error> {
        let now = self.clock.sample()?.monotonic;
        if now < self.last {
            return Err(ports::Error::Invalid);
        }
        self.last = now;
        if self.deadline.expired(now) {
            return Err(ports::Error::Deadline);
        }
        self.meter.charge(now, charge).map_err(work)?;
        Ok(now)
    }
    fn visits(&mut self, bytes: usize) -> Result<(), ports::Error> {
        self.check(Charge {
            io_bytes: bytes as u64,
            records: 1,
            ..Charge::default()
        })?;
        Ok(())
    }
    fn key<'a>(&mut self, address: &'a str) -> Result<Option<AddressKey<'a>>, ports::Error> {
        self.visits(address.len())?;
        let mut result = AddressKey {
            address,
            bytes: [0; MAX_ADDRESS],
            length: 0,
        };
        let Ok(key) = values::mailbox_key(address, &mut result.bytes) else {
            return Ok(None);
        };
        result.length = key.len();
        self.check(Charge::default())?;
        Ok(Some(result))
    }
}
fn field(name: &[u8]) -> Option<Field> {
    [
        (b"from".as_slice(), Field::From),
        (b"sender", Field::Sender),
        (b"to", Field::To),
        (b"cc", Field::Cc),
        (b"bcc", Field::Bcc),
    ]
    .into_iter()
    .find_map(|(expected, kind)| name.eq_ignore_ascii_case(expected).then_some(kind))
}
fn extent(bytes: &[u8], start: u64, end: u64) -> Result<&[u8], Error> {
    let start = usize::try_from(start).map_err(|_| Error::Headers)?;
    let end = usize::try_from(end).map_err(|_| Error::Headers)?;
    bytes.get(start..end).ok_or(Error::Headers)
}
fn trim<'a>(bytes: &'a [u8], work: &mut Work<'_>) -> Result<&'a [u8], Error> {
    let (mut start, mut end) = (0, bytes.len());
    while start < end {
        let chunk = bytes
            .get(start..end.min(start + 256))
            .ok_or(Error::Headers)?;
        work.visits(chunk.len())?;
        let count = chunk
            .iter()
            .take_while(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
            .count();
        start += count;
        if count < chunk.len() {
            break;
        }
    }
    while start < end {
        let chunk = bytes
            .get(start.max(end.saturating_sub(256))..end)
            .ok_or(Error::Headers)?;
        work.visits(chunk.len())?;
        let count = chunk
            .iter()
            .rev()
            .take_while(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
            .count();
        end -= count;
        if count < chunk.len() {
            break;
        }
    }
    work.check(Charge::default())?;
    bytes.get(start..end).ok_or(Error::Headers)
}
fn addresses<'a>(
    bytes: &'a [u8],
    kind: Field,
    work: &mut Work<'_>,
    mut visit: impl FnMut(AddressKey<'a>, &mut Work<'_>) -> Result<(), Error>,
) -> Result<(), Error> {
    let mut cursor = header_addresses::Cursor::new(bytes);
    loop {
        let now = work.check(Charge::default())?;
        match cursor.poll(now, work.meter).map_err(|error| match error {
            header_addresses::Error::Work(stop) => Error::Policy(self::work(stop)),
            _ => Error::InvalidEmail(kind),
        })? {
            header_addresses::Status::Yield
            | header_addresses::Status::BeginGroup(None)
            | header_addresses::Status::EndGroup => (),
            header_addresses::Status::BeginGroup(Some(_))
            | header_addresses::Status::Mailbox(Address::Raw(_)) => {
                return Err(Error::InvalidEmail(kind));
            }
            header_addresses::Status::Mailbox(Address::Parsed(mailbox)) => {
                let raw = bytes
                    .get(mailbox.address.start..mailbox.address.end)
                    .ok_or(Error::Headers)?;
                let raw = trim(raw, work)?;
                if raw.len() > MAX_ADDRESS {
                    return Err(Error::InvalidEmail(kind));
                }
                let address = std::str::from_utf8(raw).map_err(|_| Error::InvalidEmail(kind))?;
                let key = work.key(address)?.ok_or(Error::InvalidEmail(kind))?;
                visit(key, work)?;
            }
            header_addresses::Status::Complete => {
                work.check(Charge::default())?;
                return Ok(());
            }
        }
    }
}
/// Only the implicit envelope used by td-mail is supported. Identity must be
/// the address from the selected trusted identity, never request-supplied text.
/// Header input includes exactly its CRLF separator, never body bytes. Shared
/// address parsing supplies extents; strict SMTP mailbox validation grants no
/// recovery fallback, groups or international-address expansion.
pub fn derive<'a>(
    headers: &'a [u8],
    identity: &'a str,
    maximum: usize,
    deadline: Deadline,
    clock: &dyn Clock,
    meter: &mut Meter,
) -> Result<Envelope<'a>, Error> {
    if !(1..=1000).contains(&maximum)
        || headers.len() > crate::limits::MIB + 2
        || meter.deadline() < deadline.into()
    {
        return Err(ports::Error::Invalid.into());
    }
    let mut work = Work {
        clock,
        deadline,
        last: clock.sample()?.monotonic,
        meter,
    };
    let identity_key = work.key(identity)?.ok_or(ports::Error::Invalid)?;
    if identity_key.key()?.starts_with(b"*\0") {
        return Err(ports::Error::Invalid.into());
    }
    if !transmitted_headers::line_endings(headers, |charge| work.check(charge).map(|_| ()))? {
        return Err(Error::Headers);
    }
    let mut recipients = Vec::<AddressKey<'a>>::new();
    let mut order = Vec::<usize>::new();
    recipients
        .try_reserve_exact(maximum)
        .map_err(|_| ports::Error::Capacity)?;
    order
        .try_reserve_exact(maximum)
        .map_err(|_| ports::Error::Capacity)?;
    let mut from = None;
    let mut sender = None;
    let (mut from_seen, mut sender_seen) = (false, false);
    let mut scanner = Scanner::new(0, crate::limits::MIB as u64);
    let mut consumed = 0usize;
    loop {
        let now = work.check(Charge::default())?;
        let step = scanner
            .poll(
                headers.get(consumed..).ok_or(Error::Headers)?,
                true,
                now,
                work.meter,
            )
            .map_err(|error| match error {
                crate::mime_headers::Error::Work(stop) => Error::Policy(self::work(stop)),
                _ => Error::Headers,
            })?;
        consumed = consumed.checked_add(step.consumed).ok_or(Error::Headers)?;
        match step.status {
            Status::Yield => (),
            Status::NeedInput => return Err(Error::Headers),
            Status::Complete(end) => {
                if end.body_start != headers.len() as u64
                    || end.header_bytes.checked_add(2) != Some(end.body_start)
                    || extent(headers, end.header_bytes, end.body_start)? != b"\r\n"
                {
                    return Err(Error::Headers);
                }
                break;
            }
            Status::Field(raw) => {
                let name = extent(headers, raw.name_start, raw.name_end)?;
                work.visits(name.len().min(6) * 5)?;
                let Some(kind) = field(name) else { continue };
                let bytes = extent(headers, raw.value_start, raw.value_end)?;
                if matches!(kind, Field::From | Field::Sender) {
                    let (seen, slot) = if kind == Field::From {
                        (&mut from_seen, &mut from)
                    } else {
                        (&mut sender_seen, &mut sender)
                    };
                    if *seen {
                        return Err(Error::InvalidEmail(kind));
                    }
                    *seen = true;
                    addresses(bytes, kind, &mut work, |address, _| {
                        if slot.replace(address).is_some() {
                            return Err(Error::InvalidEmail(kind));
                        }
                        Ok(())
                    })?;
                    if slot.is_none() {
                        return Err(Error::InvalidEmail(kind));
                    }
                } else {
                    addresses(bytes, kind, &mut work, |address, work| {
                        let (mut low, mut high) = (0, order.len());
                        while low < high {
                            let middle = low + (high - low) / 2;
                            let index = *order.get(middle).ok_or(ports::Error::Invalid)?;
                            let previous = recipients.get(index).ok_or(ports::Error::Invalid)?;
                            work.visits(previous.length + address.length)?;
                            match previous.key()?.cmp(address.key()?) {
                                std::cmp::Ordering::Equal => return Ok(()),
                                std::cmp::Ordering::Less => low = middle + 1,
                                std::cmp::Ordering::Greater => high = middle,
                            }
                        }
                        if recipients.len() == maximum {
                            return Err(Error::TooManyRecipients);
                        }
                        work.visits((order.len() - low) * std::mem::size_of::<usize>())?;
                        order.push(recipients.len());
                        order
                            .get_mut(low..)
                            .ok_or(ports::Error::Invalid)?
                            .rotate_right(1);
                        recipients.push(address);
                        work.check(Charge::default())?;
                        Ok(())
                    })?;
                }
            }
        }
    }
    let from = from.ok_or(Error::InvalidEmail(Field::From))?;
    work.visits(from.length + identity_key.length)?;
    if from.key()? != identity_key.key()? {
        return Err(Error::ForbiddenFrom);
    }
    let sender = sender.unwrap_or(from);
    work.visits(sender.length + identity_key.length)?;
    if sender.key()? != identity_key.key()? {
        return Err(Error::ForbiddenFrom);
    }
    let sender = sender.address;
    if recipients.is_empty() {
        return Err(Error::NoRecipients);
    }
    work.check(Charge::default())?;
    Ok(Envelope { sender, recipients })
}

#[cfg(test)]
#[path = "submission_envelope_tests.rs"]
mod tests;
