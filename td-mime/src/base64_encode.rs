//! Bounded MIME base64 body encoding for composed text and attachments.
use crate::{
    time::Tick,
    work::{Charge, Meter, Owner, Stop},
};

pub const STEP_TRANSITIONS: usize = 256;
const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Work(Stop),
    InvalidOwner,
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Work(error) => error.fmt(f),
            Self::InvalidOwner => f.write_str("base64 encoder work owner mismatch"),
            Self::InvalidState => f.write_str("base64 encoder is incomplete or invalid"),
        }
    }
}
impl std::error::Error for Error {}
impl From<Stop> for Error {
    fn from(error: Stop) -> Self {
        Self::Work(error)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    NeedInput,
    NeedOutput,
    Yield,
    Complete,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Progress {
    pub consumed: usize,
    pub written: usize,
    pub status: Status,
}

/// One declared octet extent, under its original job meter. No source custody,
/// newline conversion, allocation, I/O or publication authority is conferred.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mime::base64_encode::Encoder>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mime::base64_encode::Encoder>();
/// ```
pub struct Encoder {
    owner: Owner,
    remaining: u64,
    bits: u32,
    octets: u8,
    column: u8,
    pending: [u8; 6],
    next: usize,
    end: usize,
    ended: bool,
    failure: Option<Error>,
}
impl Encoder {
    pub fn new(length: u64, now: Tick, meter: &mut Meter) -> Result<Self, Error> {
        meter.charge(now, Charge::default())?;
        let owner = meter.owner().ok_or(Error::InvalidOwner)?;
        Ok(Self {
            owner,
            remaining: length,
            bits: 0,
            octets: 0,
            column: 0,
            pending: [0; 6],
            next: 0,
            end: 0,
            ended: false,
            failure: None,
        })
    }
    fn check(&mut self, now: Tick, meter: &mut Meter) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if meter.owner() != Some(self.owner) {
            return Err(Error::InvalidOwner);
        }
        meter.charge(now, Charge::default())?;
        Ok(())
    }
    /// Consume only the declared extent; preserve every unconsumed suffix.
    /// Empty input is backpressure, never EOF. All output is provisional until
    /// consuming finish. Even cached completion freshly checks the job budget.
    pub fn poll(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        now: Tick,
        meter: &mut Meter,
    ) -> Result<Progress, Error> {
        let result = self.advance(input, output, now, meter);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn advance(
        &mut self,
        input: &[u8],
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
        let mut progress = Progress {
            consumed: 0,
            written: 0,
            status: Status::Yield,
        };
        for _ in 0..STEP_TRANSITIONS {
            if self.next < self.end {
                let Some(destination) = output.get_mut(progress.written) else {
                    progress.status = Status::NeedOutput;
                    return Ok(progress);
                };
                let byte = *self.pending.get(self.next).ok_or(Error::InvalidState)?;
                meter.charge(
                    now,
                    Charge {
                        output_bytes: 1,
                        ..Charge::default()
                    },
                )?;
                *destination = byte;
                self.next += 1;
                progress.written += 1;
            } else if self.ended {
                progress.status = Status::Complete;
                return Ok(progress);
            } else if self.remaining == 0 {
                if self.octets != 0 {
                    self.quartet()?;
                } else {
                    self.next = 0;
                    self.end = if self.column != 0 {
                        self.pending = [b'\r', b'\n', 0, 0, 0, 0];
                        2
                    } else {
                        0
                    };
                    self.column = 0;
                    self.ended = true;
                }
            } else if let Some(&byte) = input.get(progress.consumed) {
                meter.charge(
                    now,
                    Charge {
                        io_bytes: 1,
                        ..Charge::default()
                    },
                )?;
                self.bits = (self.bits << 8) | u32::from(byte);
                self.octets += 1;
                self.remaining -= 1;
                progress.consumed += 1;
                if self.octets == 3 {
                    self.quartet()?;
                }
            } else {
                progress.status = Status::NeedInput;
                return Ok(progress);
            }
        }
        Ok(progress)
    }
    fn quartet(&mut self) -> Result<(), Error> {
        let bits = self.bits << (8 * (3 - self.octets));
        self.pending = *b"====\r\n";
        for (n, slot) in self
            .pending
            .iter_mut()
            .take(usize::from(self.octets) + 1)
            .enumerate()
        {
            *slot = *ALPHABET
                .get(((bits >> (18 - 6 * n)) & 63) as usize)
                .ok_or(Error::InvalidState)?;
        }
        self.column += 4;
        self.next = 0;
        self.end = if self.column == 76 {
            self.column = 0;
            6
        } else {
            4
        };
        self.bits = 0;
        self.octets = 0;
        Ok(())
    }
    /// Freshly check the original meter after every byte reached the caller.
    /// The caller still owns immutable source identity and durable publication.
    pub fn finish(mut self, now: Tick, meter: &mut Meter) -> Result<(), Error> {
        self.check(now, meter)?;
        if !self.ended || self.next != self.end {
            return Err(Error::InvalidState);
        }
        Ok(())
    }
}

const _: () = assert!(std::mem::size_of::<Encoder>() <= 80);

#[cfg(test)]
#[path = "base64_encode_tests.rs"]
mod tests;
