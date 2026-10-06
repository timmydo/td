//! Bounded JSON bodyProperties argument decoding into caller-prepared strings.
use crate::{
    admission::work::{Charge, Meter, Stop},
    ports::Tick,
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Work(Stop),
    Syntax,
    Capacity,
    InvalidState,
}
impl From<Stop> for Error {
    fn from(error: Stop) -> Self {
        Self::Work(error)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Work(error) => write!(f, "body properties JSON: {error}"),
            Self::Syntax => f.write_str("invalid body properties JSON argument"),
            Self::Capacity => f.write_str("body properties JSON storage exhausted"),
            Self::InvalidState => f.write_str("invalid body properties JSON state"),
        }
    }
}
impl std::error::Error for Error {}
/// Prepare/reset caller-owned text outside the measured decoding interval.
pub struct Cell<'m> {
    buffer: Option<&'m mut String>,
}
impl<'m> Cell<'m> {
    pub fn new(buffer: &'m mut String) -> Self {
        buffer.clear();
        Self {
            buffer: Some(buffer),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Argument<'k, 'm> {
    Default,
    Explicit(&'k [&'m str]),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Complete,
}
#[derive(Clone, Copy)]
enum Phase {
    Start,
    Value {
        allow_end: bool,
    },
    String,
    Escape,
    Unicode {
        value: u16,
        digits: u8,
        high: Option<u16>,
    },
    LowSlash(u16),
    LowU(u16),
    Separator,
    End,
    Complete,
}
/// Only an omitted argument is a default; explicit null is invalid.
/// ```compile_fail,E0277
/// fn required<T: Copy>() {} required::<td_mta::body_properties::json::Cursor<'_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn required<T: Clone>() {} required::<td_mta::body_properties::json::Cursor<'_, '_, '_, '_>>();
/// ```
pub struct Cursor<'i, 'c, 'k, 'm> {
    source: Option<&'i str>,
    cells: &'c mut [Cell<'m>],
    keys: &'k mut [&'m str],
    position: usize,
    used: usize,
    phase: Phase,
    active: Option<&'m mut String>,
    failure: Option<Error>,
}
impl<'i, 'c, 'k, 'm> Cursor<'i, 'c, 'k, 'm> {
    pub fn new(
        source: Option<&'i str>,
        cells: &'c mut [Cell<'m>],
        keys: &'k mut [&'m str],
    ) -> Self {
        Self {
            source,
            cells,
            keys,
            position: 0,
            used: 0,
            phase: if source.is_none() {
                Phase::Complete
            } else {
                Phase::Start
            },
            active: None,
            failure: None,
        }
    }
    pub fn value(&self) -> Option<Argument<'_, 'm>> {
        if self.failure.is_some() || !matches!(self.phase, Phase::Complete) {
            return None;
        }
        if self.source.is_none() {
            Some(Argument::Default)
        } else {
            Some(Argument::Explicit(self.keys.get(..self.used)?))
        }
    }
    pub fn finish(self) -> Result<Argument<'k, 'm>, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !matches!(self.phase, Phase::Complete) {
            return Err(Error::InvalidState);
        }
        if self.source.is_none() {
            Ok(Argument::Default)
        } else {
            Ok(Argument::Explicit(
                self.keys.get(..self.used).ok_or(Error::InvalidState)?,
            ))
        }
    }
    pub fn poll(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.step(now, work);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn emit(&mut self, scalar: char, now: Tick, work: &mut Meter) -> Result<(), Error> {
        work.charge(
            now,
            Charge {
                output_bytes: scalar.len_utf8() as u64,
                ..Charge::default()
            },
        )?;
        let output = self.active.as_mut().ok_or(Error::InvalidState)?;
        let length = output
            .len()
            .checked_add(scalar.len_utf8())
            .ok_or(Error::Capacity)?;
        if length > output.capacity() {
            return Err(Error::Capacity);
        }
        output.push(scalar);
        Ok(())
    }
    fn step(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        if matches!(self.phase, Phase::Complete) {
            return Ok(Status::Complete);
        }
        let source = self.source.ok_or(Error::InvalidState)?;
        if self.position == source.len() {
            input_charge(now, work, 0, 1)?;
            if !matches!(self.phase, Phase::End) {
                return Err(Error::Syntax);
            }
            self.phase = Phase::Complete;
            return Ok(Status::Complete);
        }
        input_charge(now, work, 1, 1)?;
        let byte = *source
            .as_bytes()
            .get(self.position)
            .ok_or(Error::InvalidState)?;
        let mut consumed = 1;
        match self.phase {
            Phase::Start => {
                if !space(byte) {
                    if byte != b'[' {
                        return Err(Error::Syntax);
                    }
                    self.phase = Phase::Value { allow_end: true };
                }
            }
            Phase::Value { allow_end } => {
                if !space(byte) {
                    match byte {
                        b']' if allow_end => self.phase = Phase::End,
                        b'"' => {
                            if self.keys.get(self.used).is_none() {
                                return Err(Error::Capacity);
                            }
                            let cell = self.cells.get_mut(self.used).ok_or(Error::Capacity)?;
                            self.active = Some(cell.buffer.take().ok_or(Error::InvalidState)?);
                            self.phase = Phase::String;
                        }
                        _ => return Err(Error::Syntax),
                    }
                }
            }
            Phase::String => match byte {
                b'"' => {
                    let used = self.used.checked_add(1).ok_or(Error::InvalidState)?;
                    let output = self.active.take().ok_or(Error::InvalidState)?;
                    let decoded: &'m str = output.as_str();
                    *self.keys.get_mut(self.used).ok_or(Error::Capacity)? = decoded;
                    self.used = used;
                    self.phase = Phase::Separator;
                }
                b'\\' => self.phase = Phase::Escape,
                0..=31 => return Err(Error::Syntax),
                32..=127 => self.emit(char::from(byte), now, work)?,
                _ => {
                    let width = match byte {
                        0xc2..=0xdf => 2,
                        0xe0..=0xef => 3,
                        0xf0..=0xf4 => 4,
                        _ => return Err(Error::InvalidState),
                    };
                    // Boundary validation and scalar decoding revisit the leading octet.
                    input_charge(now, work, width + 1, 0)?;
                    let scalar = source
                        .get(self.position..)
                        .and_then(|tail| tail.chars().next())
                        .ok_or(Error::InvalidState)?;
                    if scalar.len_utf8() != width {
                        return Err(Error::InvalidState);
                    }
                    self.emit(scalar, now, work)?;
                    consumed = width;
                }
            },
            Phase::Escape => {
                let scalar = match byte {
                    b'"' => Some('"'),
                    b'\\' => Some('\\'),
                    b'/' => Some('/'),
                    b'b' => Some('\u{8}'),
                    b'f' => Some('\u{c}'),
                    b'n' => Some('\n'),
                    b'r' => Some('\r'),
                    b't' => Some('\t'),
                    b'u' => {
                        self.phase = Phase::Unicode {
                            value: 0,
                            digits: 0,
                            high: None,
                        };
                        None
                    }
                    _ => return Err(Error::Syntax),
                };
                if let Some(scalar) = scalar {
                    self.emit(scalar, now, work)?;
                    self.phase = Phase::String;
                }
            }
            Phase::Unicode {
                value,
                digits,
                high,
            } => {
                let digit = hex(byte).ok_or(Error::Syntax)?;
                let value = value
                    .checked_mul(16)
                    .and_then(|value| value.checked_add(u16::from(digit)))
                    .ok_or(Error::InvalidState)?;
                let digits = digits.checked_add(1).ok_or(Error::InvalidState)?;
                if digits < 4 {
                    self.phase = Phase::Unicode {
                        value,
                        digits,
                        high,
                    };
                } else if let Some(high) = high {
                    if !(0xdc00..=0xdfff).contains(&value) {
                        return Err(Error::Syntax);
                    }
                    let upper = u32::from(high)
                        .checked_sub(0xd800)
                        .and_then(|value| value.checked_mul(0x400))
                        .ok_or(Error::InvalidState)?;
                    let lower = u32::from(value)
                        .checked_sub(0xdc00)
                        .ok_or(Error::InvalidState)?;
                    let scalar = 0x10000u32
                        .checked_add(upper)
                        .and_then(|value| value.checked_add(lower))
                        .and_then(char::from_u32)
                        .ok_or(Error::Syntax)?;
                    self.emit(scalar, now, work)?;
                    self.phase = Phase::String;
                } else if (0xd800..=0xdbff).contains(&value) {
                    self.phase = Phase::LowSlash(value);
                } else {
                    let scalar = char::from_u32(u32::from(value)).ok_or(Error::Syntax)?;
                    self.emit(scalar, now, work)?;
                    self.phase = Phase::String;
                }
            }
            Phase::LowSlash(high) => {
                if byte != b'\\' {
                    return Err(Error::Syntax);
                }
                self.phase = Phase::LowU(high);
            }
            Phase::LowU(high) => {
                if byte != b'u' {
                    return Err(Error::Syntax);
                }
                self.phase = Phase::Unicode {
                    value: 0,
                    digits: 0,
                    high: Some(high),
                };
            }
            Phase::Separator => {
                if !space(byte) {
                    self.phase = match byte {
                        b',' => Phase::Value { allow_end: false },
                        b']' => Phase::End,
                        _ => return Err(Error::Syntax),
                    };
                }
            }
            Phase::End => {
                if !space(byte) {
                    return Err(Error::Syntax);
                }
            }
            Phase::Complete => return Err(Error::InvalidState),
        }
        self.position = self
            .position
            .checked_add(consumed)
            .ok_or(Error::InvalidState)?;
        Ok(Status::Yield)
    }
}
fn space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}
fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => byte.checked_sub(b'0'),
        b'a'..=b'f' => byte
            .checked_sub(b'a')
            .and_then(|value| value.checked_add(10)),
        b'A'..=b'F' => byte
            .checked_sub(b'A')
            .and_then(|value| value.checked_add(10)),
        _ => None,
    }
}
fn input_charge(now: Tick, work: &mut Meter, visits: usize, records: u64) -> Result<(), Error> {
    work.charge(
        now,
        Charge {
            io_bytes: u64::try_from(visits).map_err(|_| Error::InvalidState)?,
            records,
            ..Charge::default()
        },
    )?;
    Ok(())
}
const _: () = assert!(std::mem::size_of::<Cursor<'_, '_, '_, '_>>() <= 256);
const _: () = assert!(std::mem::size_of::<Cell<'_>>() <= 16);
#[cfg(test)]
pub use tests::probe as probe_allocations;
#[cfg(test)]
#[path = "json/tests.rs"]
mod tests;
