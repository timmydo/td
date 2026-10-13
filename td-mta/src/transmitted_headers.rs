//! Remove blind-recipient headers from a private transmitted copy.
use crate::{
    admission::work::{Charge, Meter, Stop},
    mime_headers::{Scanner, Status},
    ports::{Clock, Deadline, Error, Tick},
};

struct Output<'a, F> {
    bytes: &'a [u8],
    clock: &'a dyn Clock,
    deadline: Deadline,
    last: Tick,
    meter: &'a mut Meter,
    sink: F,
    written: u64,
}
impl<F: FnMut(&[u8]) -> Result<(), Error>> Output<'_, F> {
    fn check(&mut self, charge: Charge) -> Result<Tick, Error> {
        let now = self.clock.sample()?.monotonic;
        if now < self.last {
            return Err(Error::Invalid);
        }
        self.last = now;
        if self.deadline.expired(now) {
            return Err(Error::Deadline);
        }
        self.meter.charge(now, charge).map_err(work)?;
        Ok(now)
    }
    fn line_endings(&mut self) -> Result<(), Error> {
        let mut cr = false;
        for chunk in self.bytes.chunks(4096) {
            self.check(Charge {
                io_bytes: chunk.len() as u64,
                records: 1,
                ..Charge::default()
            })?;
            for &byte in chunk {
                if cr {
                    if byte != b'\n' {
                        return Err(Error::Invalid);
                    }
                    cr = false;
                } else {
                    match byte {
                        b'\r' => cr = true,
                        b'\n' => return Err(Error::Invalid),
                        _ => (),
                    }
                }
            }
            self.check(Charge::default())?;
        }
        if cr {
            return Err(Error::Invalid);
        }
        Ok(())
    }
    fn field(&mut self, start: usize, end: usize, skip: bool) -> Result<(), Error> {
        if skip {
            self.check(Charge::default())?;
            return Ok(());
        }
        let bytes = self.bytes.get(start..end).ok_or(Error::Invalid)?;
        for chunk in bytes.chunks(4096) {
            self.check(Charge {
                io_bytes: chunk.len() as u64,
                output_bytes: chunk.len() as u64,
                records: 1,
                ..Charge::default()
            })?;
            (self.sink)(chunk)?;
            self.written = self
                .written
                .checked_add(chunk.len() as u64)
                .ok_or(Error::Capacity)?;
            self.check(Charge::default())?;
        }
        Ok(())
    }
}
fn work(stop: Stop) -> Error {
    if stop == Stop::Deadline {
        Error::Deadline
    } else {
        Error::Capacity
    }
}
/// `headers` contains exactly the root header section and its CRLF separator,
/// at most the configured header ceiling (1 MiB) plus that separator. Body bytes
/// are streamed separately by the caller. Require CRLF-only line endings before
/// emitting anything; every other header byte is preserved.
///
/// Sink output is provisional: discard it on any failure. Success alone grants
/// no source custody, sending identity, envelope or publication authorization.
/// The caller must retain the original immutable message and meter through
/// body copying and queue publication, and reserve the complete transaction.
pub fn write(
    headers: &[u8],
    clock: &dyn Clock,
    deadline: Deadline,
    meter: &mut Meter,
    sink: impl FnMut(&[u8]) -> Result<(), Error>,
) -> Result<u64, Error> {
    if headers.len() > crate::limits::MIB + 2 || meter.deadline() < deadline.into() {
        return Err(Error::Invalid);
    }
    let mut output = Output {
        bytes: headers,
        clock,
        deadline,
        last: clock.sample()?.monotonic,
        meter,
        sink,
        written: 0,
    };
    output.line_endings()?;
    let mut scanner = Scanner::new(0, crate::limits::MIB as u64);
    let mut consumed = 0usize;
    let mut pending = None::<(usize, bool)>;
    loop {
        let now = output.check(Charge::default())?;
        let input = headers.get(consumed..).ok_or(Error::Invalid)?;
        let progress =
            scanner
                .poll(input, true, now, output.meter)
                .map_err(|error| match error {
                    crate::mime_headers::Error::Work(stop) => work(stop),
                    crate::mime_headers::Error::HeaderLimit => Error::Capacity,
                    _ => Error::Invalid,
                })?;
        consumed = consumed
            .checked_add(progress.consumed)
            .ok_or(Error::Capacity)?;
        if consumed > headers.len() {
            return Err(Error::Invalid);
        }
        output.check(Charge::default())?;
        match progress.status {
            Status::Field(field) => {
                let start = usize::try_from(field.name_start).map_err(|_| Error::Invalid)?;
                let end = usize::try_from(field.name_end).map_err(|_| Error::Invalid)?;
                if let Some((previous, skip)) = pending.take() {
                    output.field(previous, start, skip)?;
                } else if start != 0 {
                    return Err(Error::Invalid);
                }
                let name = headers.get(start..end).ok_or(Error::Invalid)?;
                let skip = if matches!(name.len(), 3 | 10) {
                    output.check(Charge {
                        io_bytes: name.len() as u64,
                        records: 1,
                        ..Charge::default()
                    })?;
                    name.eq_ignore_ascii_case(b"bcc") || name.eq_ignore_ascii_case(b"resent-bcc")
                } else {
                    false
                };
                pending = Some((start, skip));
            }
            Status::Complete(end) => {
                let fields = usize::try_from(end.header_bytes).map_err(|_| Error::Invalid)?;
                if end.body_start != headers.len() as u64
                    || fields.checked_add(2) != Some(headers.len())
                    || headers.get(fields..) != Some(b"\r\n".as_slice())
                {
                    return Err(Error::Invalid);
                }
                if let Some((start, skip)) = pending.take() {
                    output.field(start, fields, skip)?;
                } else if fields != 0 {
                    return Err(Error::Invalid);
                }
                output.field(fields, headers.len(), false)?;
                output.check(Charge::default())?;
                return Ok(output.written);
            }
            Status::Yield => (),
            Status::NeedInput => return Err(Error::Invalid),
        }
    }
}

#[cfg(test)]
#[path = "transmitted_headers_tests.rs"]
mod tests;
