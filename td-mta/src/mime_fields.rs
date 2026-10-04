//! Resident MIME field syntax; emitted extents remain provisional until Complete.
pub use crate::header_delimited::Extent;
use crate::{
    admission::work::{Charge, Meter, Stop},
    decode_work::Work,
    header_cfws, header_delimited,
    ports::Tick,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    ContentType,
    ContentDisposition,
    TransferEncoding,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Head {
    pub first: Extent,
    pub second: Option<Extent>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Parameter {
    pub name: Extent,
    pub value: Extent,
    pub quoted: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Head(Head),
    Parameter(Parameter),
    Complete,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Malformed,
    NestingLimit,
    Work(Stop),
    InterpretationLimit,
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => f.write_str("malformed MIME field"),
            Self::NestingLimit => f.write_str("MIME comment nesting limit"),
            Self::Work(error) => write!(f, "MIME syntax work: {error}"),
            Self::InterpretationLimit => f.write_str("MIME interpretation limit"),
            Self::InvalidState => f.write_str("invalid MIME syntax state"),
        }
    }
}
impl std::error::Error for Error {}
impl From<crate::decode_work::Error> for Error {
    fn from(error: crate::decode_work::Error) -> Self {
        match error {
            crate::decode_work::Error::Work(stop) => Self::Work(stop),
            crate::decode_work::Error::InterpretationLimit => Self::InterpretationLimit,
            crate::decode_work::Error::InvalidState => Self::InvalidState,
        }
    }
}
impl From<header_cfws::Error> for Error {
    fn from(error: header_cfws::Error) -> Self {
        match error {
            header_cfws::Error::Malformed => Self::Malformed,
            header_cfws::Error::NestingLimit => Self::NestingLimit,
            header_cfws::Error::Work(stop) => Self::Work(stop),
            header_cfws::Error::InterpretationLimit => Self::InterpretationLimit,
            header_cfws::Error::InvalidState => Self::InvalidState,
        }
    }
}
impl From<header_delimited::Error> for Error {
    fn from(error: header_delimited::Error) -> Self {
        match error {
            header_delimited::Error::Malformed => Self::Malformed,
            header_delimited::Error::Work(stop) => Self::Work(stop),
            header_delimited::Error::InterpretationLimit => Self::InterpretationLimit,
            header_delimited::Error::InvalidState => Self::InvalidState,
        }
    }
}
#[derive(Clone, Copy)]
enum Next {
    First,
    Slash,
    Second,
    HeadEnd,
    Name,
    Equals,
    Value,
    ParameterEnd,
}
#[derive(Clone, Copy)]
enum Token {
    First,
    Second,
    Name,
    Value,
}
#[derive(Clone, Copy)]
enum Phase {
    Gap(Next),
    Ready(Next),
    Token(Token),
    Quoted,
    End,
    Complete,
}
pub struct Cursor<'a> {
    source: &'a [u8],
    kind: Kind,
    position: usize,
    start: usize,
    phase: Phase,
    first: Extent,
    second: Option<Extent>,
    name: Extent,
    value: Extent,
    quoted: bool,
    cfws: Option<header_cfws::Cursor<'a>>,
    delimited: Option<header_delimited::Cursor<'a>>,
    failure: Option<Error>,
}
impl<'a> Cursor<'a> {
    /// Supply a complete authorized field value, excluding its final ending.
    pub const fn new(source: &'a [u8], kind: Kind) -> Self {
        Self {
            source,
            kind,
            position: 0,
            start: 0,
            phase: Phase::Gap(Next::First),
            first: Extent { start: 0, end: 0 },
            second: None,
            name: Extent { start: 0, end: 0 },
            value: Extent { start: 0, end: 0 },
            quoted: false,
            cfws: None,
            delimited: None,
            failure: None,
        }
    }
    pub fn poll(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        self.poll_with_work(now, work)
    }
    pub(crate) fn poll_with_work(
        &mut self,
        now: Tick,
        work: &mut impl Work,
    ) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.phase, Phase::Complete) {
            return Ok(Status::Complete);
        }
        let result = self.step(now, work);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn peek(&self, now: Tick, work: &mut impl Work) -> Result<Option<u8>, Error> {
        if self.position > self.source.len() {
            return Err(Error::InvalidState);
        }
        work.charge(
            now,
            Charge {
                io_bytes: u64::from(self.position < self.source.len()),
                records: 1,
                ..Charge::default()
            },
        )?;
        Ok(self.source.get(self.position).copied())
    }
    fn advance(&mut self) -> Result<(), Error> {
        self.position = self.position.checked_add(1).ok_or(Error::InvalidState)?;
        if self.position > self.source.len() {
            return Err(Error::InvalidState);
        }
        Ok(())
    }
    fn begin_token(&mut self, token: Token) {
        self.start = self.position;
        self.phase = Phase::Token(token);
    }
    fn finish_token(&mut self, token: Token) -> Result<(), Error> {
        if self.start == self.position {
            return Err(Error::Malformed);
        }
        let extent = Extent {
            start: self.start,
            end: self.position,
        };
        let next = match token {
            Token::First => {
                self.first = extent;
                if self.kind == Kind::ContentType {
                    Next::Slash
                } else {
                    Next::HeadEnd
                }
            }
            Token::Second => {
                self.second = Some(extent);
                Next::HeadEnd
            }
            Token::Name => {
                self.name = extent;
                Next::Equals
            }
            Token::Value => {
                self.value = extent;
                self.quoted = false;
                Next::ParameterEnd
            }
        };
        self.phase = Phase::Gap(next);
        Ok(())
    }
    fn end_item(&mut self, now: Tick, work: &mut impl Work) -> Result<(), Error> {
        match self.peek(now, work)? {
            None => self.phase = Phase::End,
            Some(b';') if self.kind != Kind::TransferEncoding => {
                self.advance()?;
                self.phase = Phase::Gap(Next::Name);
            }
            _ => return Err(Error::Malformed),
        }
        Ok(())
    }
    fn step(&mut self, now: Tick, work: &mut impl Work) -> Result<Status, Error> {
        work.charge(now, Charge::default())?;
        match self.phase {
            Phase::Gap(next) => {
                let child = self
                    .cfws
                    .get_or_insert_with(|| header_cfws::Cursor::new(self.source, self.position));
                if let header_cfws::Status::Complete(end) = child.poll_with_work(now, work)? {
                    self.position = end.position;
                    self.cfws = None;
                    self.phase = Phase::Ready(next);
                }
            }
            Phase::Token(token) => {
                for _ in 0..32 {
                    match self.peek(now, work)? {
                        Some(byte) if td_header::mime_token_octet(byte) => self.advance()?,
                        _ => {
                            self.finish_token(token)?;
                            break;
                        }
                    }
                }
            }
            Phase::Quoted => {
                let child = self.delimited.as_mut().ok_or(Error::InvalidState)?;
                if let header_delimited::Status::Complete(extent) =
                    child.poll_with_work(now, work)?
                {
                    self.position = extent.end;
                    self.value = extent;
                    self.quoted = true;
                    self.delimited = None;
                    self.phase = Phase::Gap(Next::ParameterEnd);
                }
            }
            Phase::Ready(next) => match next {
                Next::First => self.begin_token(Token::First),
                Next::Second => self.begin_token(Token::Second),
                Next::Name => self.begin_token(Token::Name),
                Next::Slash | Next::Equals => {
                    let expected = if matches!(next, Next::Slash) {
                        b'/'
                    } else {
                        b'='
                    };
                    if self.peek(now, work)? != Some(expected) {
                        return Err(Error::Malformed);
                    }
                    self.advance()?;
                    self.phase = Phase::Gap(if matches!(next, Next::Slash) {
                        Next::Second
                    } else {
                        Next::Value
                    });
                }
                Next::Value => {
                    if self.peek(now, work)? == Some(b'"') {
                        self.delimited = Some(header_delimited::Cursor::new(
                            self.source,
                            self.position,
                            header_delimited::Kind::QuotedString,
                        ));
                        self.phase = Phase::Quoted;
                    } else {
                        self.begin_token(Token::Value);
                    }
                }
                Next::HeadEnd => {
                    self.end_item(now, work)?;
                    return Ok(Status::Head(Head {
                        first: self.first,
                        second: self.second,
                    }));
                }
                Next::ParameterEnd => {
                    self.end_item(now, work)?;
                    return Ok(Status::Parameter(Parameter {
                        name: self.name,
                        value: self.value,
                        quoted: self.quoted,
                    }));
                }
            },
            Phase::End => {
                self.phase = Phase::Complete;
                return Ok(Status::Complete);
            }
            Phase::Complete => return Ok(Status::Complete),
        }
        Ok(Status::Yield)
    }
}

/// Retains original job/email admission across the complete field.
pub struct Budgeted<'a, 'w> {
    cursor: Cursor<'a>,
    work: &'w mut Meter,
    budget: &'w mut crate::nfc::HeaderBudget,
    credit: u8,
    failure: Option<Error>,
}
impl<'a, 'w> Budgeted<'a, 'w> {
    pub const fn new(
        source: &'a [u8],
        kind: Kind,
        work: &'w mut Meter,
        budget: &'w mut crate::nfc::HeaderBudget,
    ) -> Self {
        Self {
            cursor: Cursor::new(source, kind),
            work,
            budget,
            credit: 0,
            failure: None,
        }
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self
            .budget
            .charge(self.work, now, 0, 0, &mut self.credit)
            .map_err(crate::decode_work::Error::from)
            .map_err(Error::from);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.cursor.phase, Phase::Complete) {
            return Ok(Status::Complete);
        }
        self.check_deadline(now)?;
        let mut parsing =
            crate::decode_work::Parsing::new(self.work, self.budget, &mut self.credit);
        let result = self.cursor.poll_with_work(now, &mut parsing);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
}
#[cfg(test)]
mod tests;
