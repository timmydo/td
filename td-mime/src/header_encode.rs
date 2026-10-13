//! UTF-8 encoded words for td-mail's Subject, display names and descriptions.
//! The caller supplies the ASCII field name and final CRLF. Every nonempty
//! word starts with CRLF + space so even the first line fits independently
//! of the field name. Fold trailing syntax (such as an address) onto another
//! continuation line. Output is provisional until consuming finish succeeds.
use crate::{
    base64_encode,
    time::Tick,
    work::{Charge, Meter, Owner, Stop},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Work(Stop),
    InvalidOwner,
    InvalidText,
    Incomplete,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Work(stop) => stop.fmt(f),
            Self::InvalidOwner => f.write_str("header encoder work owner mismatch"),
            Self::InvalidText => f.write_str("header contains forbidden control or noncharacter"),
            Self::Incomplete => f.write_str("header encoding is incomplete"),
        }
    }
}
impl std::error::Error for Error {}
impl From<Stop> for Error {
    fn from(value: Stop) -> Self {
        Self::Work(value)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    NeedOutput,
    Yield,
    Complete,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Progress {
    pub written: usize,
    pub status: Status,
}

/// Borrows one immutable UTF-8 value; binds its original live work meter.
/// No header name, address syntax, filename parameter or publication authority
/// is supplied by this cursor.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mime::header_encode::Cursor<'static>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mime::header_encode::Cursor<'static>>();
/// ```
pub struct Cursor<'a> {
    input: &'a str,
    owner: Owner,
    pending: [u8; 75],
    next: usize,
    end: usize,
    failure: Option<Error>,
}
impl<'a> Cursor<'a> {
    pub fn new(input: &'a str, now: Tick, meter: &mut Meter) -> Result<Self, Error> {
        meter.charge(now, Charge::default())?;
        Ok(Self {
            input,
            owner: meter.owner().ok_or(Error::InvalidOwner)?,
            pending: [0; 75],
            next: 0,
            end: 0,
            failure: None,
        })
    }
    fn check(&self, now: Tick, meter: &mut Meter) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if meter.owner() != Some(self.owner) {
            return Err(Error::InvalidOwner);
        }
        meter.charge(now, Charge::default())?;
        Ok(())
    }
    /// Builds at most one word (45 source octets) and emits at most 75 bytes.
    /// Each poll costs one record. Validation prepays up to 49 source octets
    /// (45 plus one scalar of lookahead),
    /// and the nested body encoder its documented costs. Emitting the header
    /// bytes costs output bytes in addition to that intermediate encoding.
    pub fn poll(
        &mut self,
        output: &mut [u8],
        now: Tick,
        meter: &mut Meter,
    ) -> Result<Progress, Error> {
        let result = self.advance(output, now, meter);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn advance(
        &mut self,
        output: &mut [u8],
        now: Tick,
        meter: &mut Meter,
    ) -> Result<Progress, Error> {
        self.check(now, meter)?;
        meter.charge(
            now,
            Charge {
                records: 1,
                ..Charge::default()
            },
        )?;
        if self.next == self.end && !self.input.is_empty() {
            self.word(now, meter)?;
        }
        let mut written = 0;
        while self.next < self.end {
            let Some(destination) = output.get_mut(written) else {
                return Ok(Progress {
                    written,
                    status: Status::NeedOutput,
                });
            };
            let byte = *self.pending.get(self.next).ok_or(Error::Incomplete)?;
            meter.charge(
                now,
                Charge {
                    output_bytes: 1,
                    ..Charge::default()
                },
            )?;
            *destination = byte;
            self.next += 1;
            written += 1;
        }
        Ok(Progress {
            written,
            status: if self.input.is_empty() {
                Status::Complete
            } else {
                Status::Yield
            },
        })
    }
    fn word(&mut self, now: Tick, meter: &mut Meter) -> Result<(), Error> {
        // chars() can decode one scalar beyond the chosen word. Fund its
        // bounded lookahead before touching input, forfeiting unused credit.
        meter.charge(
            now,
            Charge {
                io_bytes: self.input.len().min(49) as u64,
                ..Charge::default()
            },
        )?;
        let mut length = 0;
        for scalar in self.input.chars() {
            let bytes = scalar.len_utf8();
            if length + bytes > 45 {
                break;
            }
            if scalar.is_control() || crate::unicode::is_noncharacter(scalar) {
                return Err(Error::InvalidText);
            }
            length += bytes;
        }
        let source = self.input.get(..length).ok_or(Error::Incomplete)?;
        let mut encoded = [0; 62];
        let mut encoder =
            base64_encode::Encoder::new(length as u64, now, meter).map_err(body_error)?;
        let step = encoder
            .poll(source.as_bytes(), &mut encoded, now, meter)
            .map_err(body_error)?;
        if step.status != base64_encode::Status::Complete || step.consumed != length {
            return Err(Error::Incomplete);
        }
        encoder.finish(now, meter).map_err(body_error)?;
        let end = step.written.checked_sub(2).ok_or(Error::Incomplete)?;
        if encoded.get(end..step.written) != Some(b"\r\n".as_slice()) {
            return Err(Error::Incomplete);
        }
        let mut position = 0;
        for bytes in [
            b"\r\n =?UTF-8?B?".as_slice(),
            encoded.get(..end).ok_or(Error::Incomplete)?,
            b"?=",
        ] {
            let stop = position + bytes.len();
            self.pending
                .get_mut(position..stop)
                .ok_or(Error::Incomplete)?
                .copy_from_slice(bytes);
            position = stop;
        }
        self.input = self.input.get(length..).ok_or(Error::Incomplete)?;
        self.next = 0;
        self.end = position;
        Ok(())
    }
    pub fn finish(self, now: Tick, meter: &mut Meter) -> Result<(), Error> {
        self.check(now, meter)?;
        if !self.input.is_empty() || self.next != self.end {
            return Err(Error::Incomplete);
        }
        Ok(())
    }
}
fn body_error(error: base64_encode::Error) -> Error {
    match error {
        base64_encode::Error::Work(stop) => Error::Work(stop),
        base64_encode::Error::InvalidOwner => Error::InvalidOwner,
        base64_encode::Error::InvalidState => Error::Incomplete,
    }
}

#[cfg(test)]
#[path = "header_encode_tests.rs"]
mod tests;

const _: () = assert!(std::mem::size_of::<Cursor<'_>>() <= 160);
