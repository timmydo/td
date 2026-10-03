//! One raw quoted string or domain literal at an authorized grammar position.
use crate::{Charge, Work};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    QuotedString,
    DomainLiteral,
}
impl Kind {
    const fn opening(self) -> u8 {
        match self {
            Self::QuotedString => b'"',
            Self::DomainLiteral => b'[',
        }
    }
    const fn closing(self) -> u8 {
        match self {
            Self::QuotedString => b'"',
            Self::DomainLiteral => b']',
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Extent {
    pub start: usize,
    pub end: usize,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Complete(Extent),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error<E> {
    Malformed,
    Work(E),
    InvalidState,
}
impl<E: std::fmt::Display> std::fmt::Display for Error<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => f.write_str("malformed delimited header token"),
            Self::Work(error) => write!(f, "delimited header token work: {error}"),
            Self::InvalidState => f.write_str("invalid delimited header token state"),
        }
    }
}
impl<E: std::error::Error> std::error::Error for Error<E> {}
#[derive(Clone, Copy)]
enum Phase {
    Opening,
    Content,
    Complete,
}
/// Source ends at the enclosing field value_end, excluding its final ending.
/// Complete validates one lexical token, not its placement or trailing field.
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {}
/// cloned::<td_header::delimited::Cursor<'_, ()>>();
/// ```
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {}
/// copied::<td_header::delimited::Cursor<'_, ()>>();
/// ```
pub struct Cursor<'a, E: Copy> {
    source: &'a [u8],
    kind: Kind,
    start: usize,
    position: usize,
    phase: Phase,
    escaped: bool,
    failure: Option<Error<E>>,
}
impl<'a, E: Copy> Cursor<'a, E> {
    pub const fn new(source: &'a [u8], start: usize, kind: Kind) -> Self {
        Self {
            source,
            kind,
            start,
            position: start,
            phase: Phase::Opening,
            escaped: false,
            failure: None,
        }
    }
    pub const fn position(&self) -> usize {
        self.position
    }
    fn extent(&self) -> Extent {
        Extent {
            start: self.start,
            end: self.position,
        }
    }
    pub fn poll(&mut self, work: &mut impl Work<Error = E>) -> Result<Status, Error<E>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.phase, Phase::Complete) {
            return Ok(Status::Complete(self.extent()));
        }
        let result = self.step(work);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn peek(&self, offset: usize, work: &mut impl Work<Error = E>) -> Result<Option<u8>, Error<E>> {
        let position = self
            .position
            .checked_add(offset)
            .ok_or(Error::InvalidState)?;
        work.charge(Charge {
            visits: u64::from(position < self.source.len()),
            ..Charge::default()
        })
        .map_err(Error::Work)?;
        Ok(self.source.get(position).copied())
    }
    fn advance(&mut self, width: usize) -> Result<(), Error<E>> {
        self.position = self
            .position
            .checked_add(width)
            .ok_or(Error::InvalidState)?;
        if self.position > self.source.len() {
            return Err(Error::InvalidState);
        }
        Ok(())
    }
    fn scalar_width(&self, byte: u8, work: &mut impl Work<Error = E>) -> Result<usize, Error<E>> {
        let width = match byte {
            0..=127 => return Ok(1),
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => return Err(Error::Malformed),
        };
        let available = self
            .source
            .len()
            .checked_sub(self.position)
            .ok_or(Error::InvalidState)?
            .min(width);
        // The UTF-8 validator rereads the already inspected lead octet.
        work.charge(Charge {
            visits: available as u64,
            ..Charge::default()
        })
        .map_err(Error::Work)?;
        let end = self
            .position
            .checked_add(available)
            .ok_or(Error::InvalidState)?;
        let bytes = self
            .source
            .get(self.position..end)
            .ok_or(Error::InvalidState)?;
        if available != width || std::str::from_utf8(bytes).is_err() {
            return Err(Error::Malformed);
        }
        Ok(width)
    }
    fn fold_width(&self, byte: u8, work: &mut impl Work<Error = E>) -> Result<usize, Error<E>> {
        let width = if byte == b'\r' {
            if self.peek(1, work)? != Some(b'\n') {
                return Err(Error::Malformed);
            }
            2
        } else {
            1
        };
        if !matches!(self.peek(width, work)?, Some(b' ' | b'\t')) {
            return Err(Error::Malformed);
        }
        Ok(width)
    }
    fn step(&mut self, work: &mut impl Work<Error = E>) -> Result<Status, Error<E>> {
        if self.position > self.source.len() {
            return Err(Error::InvalidState);
        }
        for _ in 0..32 {
            work.charge(Charge {
                records: 1,
                ..Charge::default()
            })
            .map_err(Error::Work)?;
            let byte = self.peek(0, work)?.ok_or(Error::Malformed)?;
            if matches!(self.phase, Phase::Opening) {
                if byte != self.kind.opening() {
                    return Err(Error::Malformed);
                }
                self.advance(1)?;
                self.phase = Phase::Content;
                continue;
            }
            if self.escaped {
                let width = self.scalar_width(byte, work)?;
                self.advance(width)?;
                self.escaped = false;
                continue;
            }
            if byte == self.kind.closing() {
                self.advance(1)?;
                self.phase = Phase::Complete;
                return Ok(Status::Complete(self.extent()));
            }
            match byte {
                b'\\' => {
                    self.advance(1)?;
                    self.escaped = true;
                }
                b'\r' | b'\n' => {
                    let width = self.fold_width(byte, work)?;
                    self.advance(width)?;
                }
                0 => return Err(Error::Malformed),
                b'[' if self.kind == Kind::DomainLiteral => return Err(Error::Malformed),
                _ => {
                    let width = self.scalar_width(byte, work)?;
                    self.advance(width)?;
                }
            }
        }
        Ok(Status::Yield)
    }
}
