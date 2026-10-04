//! Compatibility words require the selected original raw value.
use super::super::Error;
use crate::{
    admission::work::Charge,
    decode_work::Work,
    encoded_word::{self, Context, Word},
    mime_fields::Parameter,
    ports::Tick,
};
use encoded_word::decode::Status;
#[derive(Clone, Copy)]
enum Phase {
    Start,
    Boundary,
    Literal,
    Gap,
    Candidate,
    Recognize,
    EmitGap,
    Word,
    Complete,
}
pub(super) struct Cursor<'a> {
    raw: &'a [u8],
    quoted: bool,
    end: usize,
    position: usize,
    scan: usize,
    token_start: usize,
    gap_end: usize,
    phase: Phase,
    word: Option<encoded_word::decode::Cursor<'a>>,
    problem: bool,
}
impl<'a> Cursor<'a> {
    pub(super) fn new(source: &'a [u8], parameter: Parameter) -> Result<Self, Error> {
        let raw = source
            .get(parameter.value.start..parameter.value.end)
            .ok_or(Error::InvalidState)?;
        let edge = usize::from(parameter.quoted);
        let end = raw.len().checked_sub(edge).ok_or(Error::InvalidState)?;
        if edge > end {
            return Err(Error::InvalidState);
        }
        Ok(Self {
            raw,
            quoted: parameter.quoted,
            end,
            position: edge,
            scan: edge,
            token_start: edge,
            gap_end: edge,
            phase: Phase::Start,
            word: None,
            problem: false,
        })
    }
    pub(super) const fn is_encoding_problem(&self) -> bool {
        self.problem
    }
    fn byte(&self, at: usize, now: Tick, work: &mut impl Work) -> Result<Option<u8>, Error> {
        if at > self.end {
            return Err(Error::InvalidState);
        }
        work.charge(
            now,
            Charge {
                io_bytes: u64::from(at < self.end),
                ..Charge::default()
            },
        )?;
        Ok(if at == self.end {
            None
        } else {
            Some(*self.raw.get(at).ok_or(Error::InvalidState)?)
        })
    }
    fn atom(
        &self,
        at: usize,
        now: Tick,
        work: &mut impl Work,
    ) -> Result<Option<td_header::projection::Octet>, Error> {
        td_header::projection::atom(at, self.quoted, |at| self.byte(at, now, work)).map_err(
            |error| match error {
                td_header::projection::Error::Read(error) => error,
                td_header::projection::Error::IncompletePair
                | td_header::projection::Error::InvalidState => Error::InvalidState,
            },
        )
    }
    fn candidate(&mut self, at: usize) {
        self.token_start = at;
        self.scan = at;
        self.phase = Phase::Candidate;
    }
    fn reject(&mut self) {
        self.gap_end = self.token_start;
        self.phase = if self.position < self.gap_end {
            Phase::EmitGap
        } else {
            Phase::Literal
        };
    }
    fn literal(&mut self, now: Tick, work: &mut impl Work) -> Result<Status, Error> {
        let Some(first) = self.atom(self.position, now, work)? else {
            self.phase = Phase::Complete;
            return Ok(Status::Complete);
        };
        let width = match first.value {
            0..=0x7f => 1,
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => return Err(Error::InvalidState),
        };
        let mut next = first.next;
        let mut bytes = [first.value, 0, 0, 0];
        for cell in bytes.get_mut(1..width).ok_or(Error::InvalidState)? {
            let octet = self.atom(next, now, work)?.ok_or(Error::InvalidState)?;
            *cell = octet.value;
            next = octet.next;
        }
        // Verify the local UTF-8 spelling separately from source projection.
        work.charge(
            now,
            Charge {
                io_bytes: width as u64,
                ..Charge::default()
            },
        )?;
        let value = std::str::from_utf8(bytes.get(..width).ok_or(Error::InvalidState)?)
            .map_err(|_| Error::InvalidState)?
            .chars()
            .next()
            .ok_or(Error::InvalidState)?;
        self.position = next;
        if !matches!(self.phase, Phase::EmitGap) {
            self.phase = if !first.escaped && matches!(value, ' ' | '\t') {
                Phase::Boundary
            } else {
                Phase::Literal
            };
        }
        Ok(super::filter(value, &mut self.problem).map_or(Status::Yield, Status::Scalar))
    }
    pub(super) fn poll(&mut self, now: Tick, work: &mut impl Work) -> Result<Status, Error> {
        if matches!(self.phase, Phase::Complete) {
            return Ok(Status::Complete);
        }
        work.charge(
            now,
            Charge {
                records: 1,
                ..Charge::default()
            },
        )?;
        match self.phase {
            Phase::Start => {
                if self.quoted {
                    work.charge(
                        now,
                        Charge {
                            io_bytes: 2,
                            ..Charge::default()
                        },
                    )?;
                    if self.raw.first() != Some(&b'"') || self.raw.get(self.end) != Some(&b'"') {
                        return Err(Error::InvalidState);
                    }
                }
                self.phase = Phase::Boundary;
            }
            Phase::Boundary => match self.byte(self.position, now, work)? {
                Some(b'=') if self.quoted => self.candidate(self.position),
                _ => return self.literal(now, work),
            },
            Phase::Literal | Phase::EmitGap => {
                if matches!(self.phase, Phase::EmitGap) && self.position == self.gap_end {
                    self.phase = Phase::Literal;
                } else {
                    return self.literal(now, work);
                }
            }
            Phase::Gap => match self.atom(self.scan, now, work)? {
                Some(octet) if !octet.escaped && matches!(octet.value, b' ' | b'\t') => {
                    self.scan = octet.next
                }
                Some(octet) if !octet.escaped && octet.value == b'=' => self.candidate(self.scan),
                _ => {
                    self.token_start = self.scan;
                    self.reject();
                }
            },
            Phase::Candidate => match self.byte(self.scan, now, work)? {
                None | Some(b' ' | b'\t') => self.phase = Phase::Recognize,
                Some(b'\r' | b'\n') => match self.atom(self.scan, now, work)? {
                    Some(octet) if !octet.escaped && matches!(octet.value, b' ' | b'\t') => {
                        self.phase = Phase::Recognize
                    }
                    _ => self.reject(),
                },
                Some(b'\\') => self.reject(),
                Some(_) => {
                    self.scan = self.scan.checked_add(1).ok_or(Error::InvalidState)?;
                    if self
                        .scan
                        .checked_sub(self.token_start)
                        .ok_or(Error::InvalidState)?
                        > 75
                    {
                        self.reject();
                    }
                }
            },
            Phase::Recognize => {
                let token = self
                    .raw
                    .get(self.token_start..self.scan)
                    .ok_or(Error::InvalidState)?;
                match Word::recognize_with_work(token, Context::Text, now, work)? {
                    Some(word) => {
                        self.word = Some(encoded_word::decode::Cursor::new(word));
                        self.position = self.scan;
                        self.phase = Phase::Word;
                    }
                    None => self.reject(),
                }
            }
            Phase::Word => {
                let word = self.word.as_mut().ok_or(Error::InvalidState)?;
                let status = word.poll_with_work(now, work)?;
                self.problem |= word.is_encoding_problem();
                if status == Status::Complete {
                    self.word = None;
                    self.scan = self.position;
                    self.phase = Phase::Gap;
                } else {
                    return Ok(status);
                }
            }
            Phase::Complete => return Err(Error::InvalidState),
        }
        Ok(Status::Yield)
    }
}
