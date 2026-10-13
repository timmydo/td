//! Plain-text RFC 5322 composition for td-mail's structured draft creation.
//! Bytes sent to the sink are provisional: the caller owns bounded private
//! staging, all-or-nothing publication and account/device authorization.
use crate::{
    admission::work::{Charge, Meter},
    bounded::TextBuffer,
    config::values,
    ports::{Clock, Error, Tick},
};
use td_mime::{base64_encode, header_encode, header_message_ids as ids, work::Stop};

#[derive(Clone, Copy)]
pub struct Address<'a> {
    pub name: &'a str,
    pub email: &'a str,
}
pub struct Message<'a> {
    pub from: Address<'a>,
    pub to: &'a [Address<'a>],
    pub cc: &'a [Address<'a>],
    pub bcc: &'a [Address<'a>],
    pub reply_to: &'a [Address<'a>],
    pub subject: &'a str,
    pub message_id: &'a str,
    pub in_reply_to: &'a [&'a str],
    pub references: &'a [&'a str],
    pub sent_at_ms: i64,
    pub text: &'a str,
}
struct Writer<'a, F> {
    sink: F,
    clock: &'a dyn Clock,
    meter: &'a mut Meter,
    last: Tick,
    length: u64,
    maximum: u64,
}
impl<F: FnMut(&[u8]) -> Result<(), Error>> Writer<'_, F> {
    fn tick(&mut self) -> Result<Tick, Error> {
        let now = self.clock.sample()?.monotonic;
        if now < self.last {
            return Err(Error::Invalid);
        }
        self.last = now;
        self.meter
            .charge(now, Charge::default())
            .map_err(work_error)?;
        Ok(now)
    }
    fn emit(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let now = self.tick()?;
        let next = self
            .length
            .checked_add(bytes.len() as u64)
            .filter(|n| *n <= self.maximum)
            .ok_or(Error::Quota)?;
        self.meter
            .charge(
                now,
                Charge {
                    output_bytes: bytes.len() as u64,
                    records: 1,
                    ..Charge::default()
                },
            )
            .map_err(work_error)?;
        (self.sink)(bytes)?;
        self.length = next;
        self.tick()?;
        Ok(())
    }
    fn text(&mut self, value: &str) -> Result<(), Error> {
        let now = self.tick()?;
        let mut cursor =
            header_encode::Cursor::new(value, now, self.meter).map_err(header_error)?;
        loop {
            let now = self.tick()?;
            let mut output = [0; 75];
            let step = cursor
                .poll(&mut output, now, self.meter)
                .map_err(header_error)?;
            self.emit(output.get(..step.written).ok_or(Error::Invalid)?)?;
            if step.status == header_encode::Status::Complete {
                break;
            }
        }
        let now = self.tick()?;
        cursor.finish(now, self.meter).map_err(header_error)
    }
    fn addresses(&mut self, name: &[u8], addresses: &[Address<'_>]) -> Result<(), Error> {
        if addresses.len() > 1000 {
            return Err(Error::Capacity);
        }
        if addresses.is_empty() {
            return Ok(());
        }
        self.emit(name)?;
        for (index, address) in addresses.iter().enumerate() {
            let now = self.tick()?;
            self.meter
                .charge(
                    now,
                    Charge {
                        io_bytes: address.email.len() as u64,
                        records: 1,
                        ..Charge::default()
                    },
                )
                .map_err(work_error)?;
            values::mailbox_key(address.email, &mut [0; crate::format::row::MAX_ADDRESS])
                .map_err(|_| Error::Invalid)?;
            if index != 0 {
                self.emit(b",")?;
            }
            self.text(address.name)?;
            self.emit(b"\r\n <")?;
            self.emit(address.email.as_bytes())?;
            self.emit(b">")?;
        }
        self.emit(b"\r\n")
    }
    fn identifiers(&mut self, name: &[u8], identifiers: &[&str]) -> Result<(), Error> {
        if identifiers.len() > 1000 {
            return Err(Error::Capacity);
        }
        if identifiers.is_empty() {
            return Ok(());
        }
        self.emit(name)?;
        for id in identifiers {
            if id.is_empty() || id.len() > 995 {
                return Err(Error::Invalid);
            }
            let now = self.tick()?;
            self.meter
                .charge(
                    now,
                    Charge {
                        io_bytes: id.len() as u64,
                        records: 1,
                        ..Charge::default()
                    },
                )
                .map_err(work_error)?;
            // The inbound cursor also tolerates obsolete quoted forms. Generated
            // IDs require visible ASCII, a dot-atom left side and no escapes.
            let mut local = true;
            for byte in id.bytes() {
                if !(33..=126).contains(&byte) || byte == b'\\' || (local && byte == b'"') {
                    return Err(Error::Invalid);
                }
                if byte == b'@' {
                    local = false;
                }
            }
            if local {
                return Err(Error::Invalid);
            }
            let mut raw = [0; 997];
            let end = id.len() + 1;
            *raw.first_mut().ok_or(Error::Invalid)? = b'<';
            raw.get_mut(1..end)
                .ok_or(Error::Invalid)?
                .copy_from_slice(id.as_bytes());
            *raw.get_mut(end).ok_or(Error::Invalid)? = b'>';
            let source = raw.get(..end + 1).ok_or(Error::Invalid)?;
            let mut cursor = ids::Cursor::new(source, ids::Mode::Strict);
            let (mut begin, mut finish, mut bytes) = (0usize, 0usize, 0usize);
            loop {
                let now = self.tick()?;
                match cursor.poll(now, self.meter).map_err(id_error)? {
                    ids::Status::Yield => (),
                    ids::Status::Begin => begin += 1,
                    ids::Status::End => finish += 1,
                    ids::Status::Part(part) => {
                        bytes = bytes
                            .checked_add(part.end.checked_sub(part.start).ok_or(Error::Invalid)?)
                            .ok_or(Error::Capacity)?
                    }
                    ids::Status::Complete => break,
                }
            }
            if begin != 1 || finish != 1 || bytes != id.len() {
                return Err(Error::Invalid);
            }
            self.emit(b"\r\n ")?;
            self.emit(source)?;
        }
        self.emit(b"\r\n")
    }
    fn body(&mut self, text: &str) -> Result<(), Error> {
        let mut length = 0u64;
        let mut previous_cr = false;
        for chunk in text.as_bytes().chunks(1024) {
            let now = self.tick()?;
            self.meter
                .charge(
                    now,
                    Charge {
                        io_bytes: chunk.len() as u64,
                        records: 1,
                        ..Charge::default()
                    },
                )
                .map_err(work_error)?;
            for byte in chunk {
                let amount = if *byte == b'\n' && previous_cr {
                    0
                } else if matches!(*byte, b'\n' | b'\r') {
                    2
                } else {
                    1
                };
                length = length.checked_add(amount).ok_or(Error::Capacity)?;
                previous_cr = *byte == b'\r';
            }
        }
        let now = self.tick()?;
        let mut encoder =
            base64_encode::Encoder::new(length, now, self.meter).map_err(body_error)?;
        let mut source = text.as_bytes();
        let mut pending_lf = false;
        let mut input = [0; 1024];
        let (mut position, mut end) = (0, 0);
        loop {
            if position == end {
                position = 0;
                end = 0;
                let now = self.tick()?;
                self.meter
                    .charge(
                        now,
                        Charge {
                            io_bytes: source.len().min(1025) as u64,
                            records: 1,
                            ..Charge::default()
                        },
                    )
                    .map_err(work_error)?;
                for destination in &mut input {
                    if pending_lf {
                        *destination = b'\n';
                        pending_lf = false;
                    } else if let Some((&byte, rest)) = source.split_first() {
                        source = rest;
                        if byte == b'\r' || byte == b'\n' {
                            *destination = b'\r';
                            pending_lf = true;
                            if byte == b'\r' {
                                source = source.strip_prefix(b"\n").unwrap_or(source);
                            }
                        } else {
                            *destination = byte;
                        }
                    } else {
                        break;
                    }
                    end += 1;
                }
            }
            let now = self.tick()?;
            let mut output = [0; 256];
            let step = encoder
                .poll(
                    input.get(position..end).ok_or(Error::Invalid)?,
                    &mut output,
                    now,
                    self.meter,
                )
                .map_err(body_error)?;
            position += step.consumed;
            self.emit(output.get(..step.written).ok_or(Error::Invalid)?)?;
            if step.status == base64_encode::Status::Complete {
                break;
            }
            if step.status == base64_encode::Status::NeedInput
                && source.is_empty()
                && !pending_lf
                && position == end
            {
                return Err(Error::Invalid);
            }
        }
        let now = self.tick()?;
        encoder.finish(now, self.meter).map_err(body_error)
    }
}

/// Compose one plain-text message. Retain supplied IDs/date exactly; generation
/// happens once upstream. The sink must stay private and discard all bytes on
/// any error. This neither creates an Email nor takes delivery responsibility.
pub fn compose(
    message: &Message<'_>,
    maximum: u64,
    clock: &dyn Clock,
    meter: &mut Meter,
    sink: impl FnMut(&[u8]) -> Result<(), Error>,
) -> Result<u64, Error> {
    let last = clock.sample()?.monotonic;
    let mut writer = Writer {
        sink,
        clock,
        meter,
        last,
        length: 0,
        maximum,
    };
    writer.addresses(b"From:", std::slice::from_ref(&message.from))?;
    for (name, addresses) in [
        (b"To:".as_slice(), message.to),
        (b"Cc:", message.cc),
        (b"Bcc:", message.bcc),
        (b"Reply-To:", message.reply_to),
    ] {
        writer.addresses(name, addresses)?;
    }
    writer.emit(b"Subject:")?;
    writer.text(message.subject)?;
    writer.emit(b"\r\n")?;
    writer.identifiers(b"Message-ID:", &[message.message_id])?;
    writer.identifiers(b"In-Reply-To:", message.in_reply_to)?;
    writer.identifiers(b"References:", message.references)?;
    let date = td_civil::unix_to_civil_utc_checked(message.sent_at_ms.div_euclid(1000))
        .filter(|d| (1900..=9999).contains(&d.year))
        .ok_or(Error::Invalid)?;
    const MONTHS: &[&str] = &[
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    const DAYS: &[&str] = &["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let weekday = (message.sent_at_ms.div_euclid(86_400_000) + 4).rem_euclid(7) as usize;
    let month = MONTHS
        .get(
            usize::from(date.month)
                .checked_sub(1)
                .ok_or(Error::Invalid)?,
        )
        .ok_or(Error::Invalid)?;
    let day = DAYS.get(weekday).ok_or(Error::Invalid)?;
    let mut buffer = [0; 64];
    let mut output = TextBuffer::new(&mut buffer);
    output
        .format(format_args!(
            "Date: {day}, {:02} {month} {} {:02}:{:02}:{:02} +0000\r\n",
            date.day, date.year, date.hour, date.minute, date.second
        ))
        .map_err(|_| Error::Capacity)?;
    writer.emit(output.as_bytes().map_err(|_| Error::Capacity)?)?;
    writer.emit(b"MIME-Version: 1.0\r\nContent-Type: text/plain; charset=UTF-8\r\nContent-Transfer-Encoding: base64\r\n\r\n")?;
    writer.body(message.text)?;
    writer.tick()?;
    Ok(writer.length)
}
fn work_error(stop: Stop) -> Error {
    if stop == Stop::Deadline {
        Error::Deadline
    } else {
        Error::Capacity
    }
}
fn header_error(error: header_encode::Error) -> Error {
    match error {
        header_encode::Error::Work(stop) => work_error(stop),
        _ => Error::Invalid,
    }
}
fn body_error(error: base64_encode::Error) -> Error {
    match error {
        base64_encode::Error::Work(stop) => work_error(stop),
        _ => Error::Invalid,
    }
}
fn id_error(error: ids::Error) -> Error {
    match error {
        ids::Error::Work(stop) => work_error(stop),
        _ => Error::Invalid,
    }
}

#[cfg(test)]
#[path = "compose_text_tests.rs"]
mod tests;
