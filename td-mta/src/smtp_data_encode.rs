//! Strict SMTP DATA transparency without any end-of-data terminator.
use td_mime::{
    time::Tick,
    work::{Charge, Meter, Owner, Stop},
};

pub const STEP_TRANSITIONS: usize = 256;
pub const MAX_LINE_CONTENT: usize = 998;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Body {
    SevenBit,
    EightBit,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Work(Stop),
    InvalidOwner,
    InvalidData,
    InvalidState,
}
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
/// One immutable declared source extent under its original work meter.
/// This does not validate MIME/header semantics or authorize transport/body mode.
/// Output is provisional until finish; on failure discard it and abort transport.
/// No method returns or emits a DATA terminator, including after completion.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::smtp_data_encode::Encoder>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::smtp_data_encode::Encoder>();
/// ```
pub struct Encoder {
    owner: Owner,
    remaining: u64,
    body: Body,
    column: usize,
    cr: bool,
    pending: [u8; 2],
    next: usize,
    end: usize,
    complete: bool,
    failure: Option<Error>,
}
impl Encoder {
    /// EightBit is a caller-supplied policy choice, not evidence of negotiation.
    /// A nonempty source must end in CRLF; line endings are never repaired.
    pub fn new(length: u64, body: Body, now: Tick, meter: &mut Meter) -> Result<Self, Error> {
        meter.charge(now, Charge::default())?;
        let owner = meter.owner().ok_or(Error::InvalidOwner)?;
        Ok(Self {
            owner,
            remaining: length,
            body,
            column: 0,
            cr: false,
            pending: [0; 2],
            next: 0,
            end: 0,
            complete: false,
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
    /// Consume only the declared extent; preserve all trailing input externally.
    /// Empty input/output is backpressure. Every call rechecks the original meter.
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
            } else if self.complete {
                progress.status = Status::Complete;
                return Ok(progress);
            } else if self.remaining == 0 {
                if self.cr || self.column != 0 {
                    return Err(Error::InvalidData);
                }
                self.complete = true;
            } else if let Some(&byte) = input.get(progress.consumed) {
                meter.charge(
                    now,
                    Charge {
                        io_bytes: 1,
                        ..Charge::default()
                    },
                )?;
                self.byte(byte)?;
                self.remaining -= 1;
                progress.consumed += 1;
            } else {
                progress.status = Status::NeedInput;
                return Ok(progress);
            }
        }
        Ok(progress)
    }
    fn byte(&mut self, byte: u8) -> Result<(), Error> {
        let dot = self.column == 0 && !self.cr && byte == b'.';
        if self.cr {
            if byte != b'\n' {
                return Err(Error::InvalidData);
            }
            self.cr = false;
            self.column = 0;
        } else {
            match byte {
                0 | b'\n' => return Err(Error::InvalidData),
                b'\r' => self.cr = true,
                _ => {
                    if (self.body == Body::SevenBit && byte >= 128)
                        || self.column == MAX_LINE_CONTENT
                    {
                        return Err(Error::InvalidData);
                    }
                    self.column += 1;
                }
            }
        }
        self.pending = [byte, byte];
        self.next = 0;
        self.end = if dot { 2 } else { 1 };
        Ok(())
    }
    /// Fresh budget check; completion never authorizes terminator transmission.
    pub fn finish(self, now: Tick, meter: &mut Meter) -> Result<(), Error> {
        self.check(now, meter)?;
        if !self.complete || self.next != self.end || self.remaining != 0 {
            return Err(Error::InvalidState);
        }
        Ok(())
    }
}
const _: () = assert!(std::mem::size_of::<Encoder>() <= 96);
#[cfg(test)]
#[path = "smtp_data_encode_tests.rs"]
mod tests;
