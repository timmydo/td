//! Optional structured-header CFWS; the enclosing grammar authorizes placement.
use crate::{Charge, Work};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Comment {
    pub start: usize,
    pub end: usize,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct End {
    pub position: usize,
    pub consumed: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Comment(Comment),
    Yield,
    Complete(End),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error<E> {
    Malformed,
    NestingLimit,
    Work(E),
    InvalidState,
}
impl<E: std::fmt::Display> std::fmt::Display for Error<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => f.write_str("malformed header comment"),
            Self::NestingLimit => f.write_str("header comment nesting limit"),
            Self::Work(error) => write!(f, "header comment work: {error}"),
            Self::InvalidState => f.write_str("invalid header comment state"),
        }
    }
}
impl<E: std::error::Error> std::error::Error for Error<E> {}
/// Scans one optional CFWS run, leaving the first non-CFWS byte unconsumed.
/// Source ends at the enclosing field value_end, excluding its final ending.
/// This lexical helper does not establish raw-file field boundaries.
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {}
/// cloned::<td_header::cfws::Cursor<'_, ()>>();
/// ```
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {}
/// copied::<td_header::cfws::Cursor<'_, ()>>();
/// ```
pub struct Cursor<'a, E: Copy> {
    source: &'a [u8],
    start: usize,
    position: usize,
    comment_start: usize,
    depth: u8,
    escaped: bool,
    complete: bool,
    failure: Option<Error<E>>,
}
impl<'a, E: Copy> Cursor<'a, E> {
    pub const fn new(source: &'a [u8], start: usize) -> Self {
        Self {
            source,
            start,
            position: start,
            comment_start: start,
            depth: 0,
            escaped: false,
            complete: false,
            failure: None,
        }
    }
    pub const fn position(&self) -> usize {
        self.position
    }
    fn end(&self) -> End {
        End {
            position: self.position,
            consumed: self.position != self.start,
        }
    }
    pub fn poll(&mut self, work: &mut impl Work<Error = E>) -> Result<Status, Error<E>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.complete {
            return Ok(Status::Complete(self.end()));
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
    fn advance(&mut self, count: usize) -> Result<(), Error<E>> {
        self.position = self
            .position
            .checked_add(count)
            .ok_or(Error::InvalidState)?;
        if self.position > self.source.len() {
            return Err(Error::InvalidState);
        }
        Ok(())
    }
    fn fold(&self, byte: u8, work: &mut impl Work<Error = E>) -> Result<Option<usize>, Error<E>> {
        let width = if byte == b'\r' {
            if self.peek(1, work)? != Some(b'\n') {
                return Ok(None);
            }
            2
        } else {
            1
        };
        Ok(matches!(self.peek(width, work)?, Some(b' ' | b'\t')).then_some(width))
    }
    fn character(&self, byte: u8, work: &mut impl Work<Error = E>) -> Result<usize, Error<E>> {
        let width = match byte {
            0..=127 => return Ok(1),
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => return Err(Error::Malformed),
        };
        // The bounded UTF-8 validator rereads the lead already inspected above.
        let available = self
            .source
            .len()
            .checked_sub(self.position)
            .ok_or(Error::InvalidState)?
            .min(width);
        work.charge(Charge {
            visits: available as u64,
            ..Charge::default()
        })
        .map_err(Error::Work)?;
        let end = self
            .position
            .checked_add(available)
            .ok_or(Error::InvalidState)?;
        let text = self
            .source
            .get(self.position..end)
            .ok_or(Error::InvalidState)?;
        if available != width || std::str::from_utf8(text).is_err() {
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
            let Some(byte) = self.peek(0, work)? else {
                if self.depth != 0 {
                    return Err(Error::Malformed);
                }
                self.complete = true;
                return Ok(Status::Complete(self.end()));
            };
            if self.escaped {
                let width = self.character(byte, work)?;
                self.advance(width)?;
                self.escaped = false;
                continue;
            }
            if matches!(byte, b' ' | b'\t') {
                self.advance(1)?;
                continue;
            }
            if matches!(byte, b'\r' | b'\n') {
                if let Some(width) = self.fold(byte, work)? {
                    self.advance(width)?;
                    continue;
                }
                if self.depth != 0 {
                    return Err(Error::Malformed);
                }
                self.complete = true;
                return Ok(Status::Complete(self.end()));
            }
            if byte == b'(' {
                if self.depth == 32 {
                    return Err(Error::NestingLimit);
                }
                if self.depth == 0 {
                    self.comment_start = self.position;
                }
                self.depth = self.depth.checked_add(1).ok_or(Error::InvalidState)?;
                self.advance(1)?;
                continue;
            }
            if self.depth == 0 {
                self.complete = true;
                return Ok(Status::Complete(self.end()));
            }
            match byte {
                b')' => {
                    self.depth = self.depth.checked_sub(1).ok_or(Error::InvalidState)?;
                    self.advance(1)?;
                    if self.depth == 0 {
                        return Ok(Status::Comment(Comment {
                            start: self.comment_start,
                            end: self.position,
                        }));
                    }
                }
                b'\\' => {
                    self.escaped = true;
                    self.advance(1)?;
                }
                0 => return Err(Error::Malformed),
                _ => {
                    let width = self.character(byte, work)?;
                    self.advance(width)?;
                }
            }
        }
        Ok(Status::Yield)
    }
}
