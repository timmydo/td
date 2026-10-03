//! Bounded NFC over resident UTF-8 and authorized header scalar sources.
use crate::{
    admission::work::{Charge, Meter, Stop},
    ports::Tick,
    unicode::{self, Decomposition},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Work(Stop),
    InterpretationLimit,
    InvalidState,
    InvalidTable,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Work(error) => write!(f, "NFC work: {error}"),
            Self::InterpretationLimit => f.write_str("NFC interpretation limit"),
            Self::InvalidState => f.write_str("invalid NFC state"),
            Self::InvalidTable => f.write_str("invalid NFC table"),
        }
    }
}
impl std::error::Error for Error {}

/// One aggregate budget shared by all header projections of an email.
pub struct HeaderBudget {
    bytes: u64,
    steps: u64,
    exhausted: bool,
}
impl Default for HeaderBudget {
    fn default() -> Self {
        Self::new()
    }
}
impl HeaderBudget {
    pub const fn new() -> Self {
        Self {
            bytes: 16 * 1024 * 1024,
            steps: 16_000_000,
            exhausted: false,
        }
    }
    pub const fn source_bytes_remaining(&self) -> u64 {
        self.bytes
    }
    pub const fn steps_remaining(&self) -> u64 {
        self.steps
    }
    pub(crate) fn charge(
        &mut self,
        work: &mut Meter,
        now: Tick,
        bytes: u64,
        steps: u64,
        credit: &mut u8,
    ) -> Result<(), Error> {
        if self.exhausted {
            return Err(Error::InterpretationLimit);
        }
        work.charge(now, Charge::default()).map_err(Error::Work)?;
        let (Some(next_bytes), Some(next_steps)) =
            (self.bytes.checked_sub(bytes), self.steps.checked_sub(steps))
        else {
            self.exhausted = true;
            return Err(Error::InterpretationLimit);
        };
        let records = steps.saturating_sub(u64::from(*credit)).div_ceil(16);
        let prepaid = records.checked_mul(16).ok_or(Error::InvalidState)?;
        let next_credit = u64::from(*credit)
            .checked_add(prepaid)
            .and_then(|value| value.checked_sub(steps))
            .and_then(|value| u8::try_from(value).ok())
            .ok_or(Error::InvalidState)?;
        work.charge(
            now,
            Charge {
                io_bytes: bytes,
                records,
                ..Charge::default()
            },
        )
        .map_err(Error::Work)?;
        self.bytes = next_bytes;
        self.steps = next_steps;
        *credit = next_credit;
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct Cell {
    value: char,
    class: u8,
}
impl Cell {
    const EMPTY: Self = Self {
        value: '\0',
        class: 0,
    };
}
/// Allocate/touch once before admission and lend exclusively to one cursor.
pub struct Scratch {
    cells: [Cell; 256],
    counts: [u32; 256],
}
impl Default for Scratch {
    fn default() -> Self {
        Self::new()
    }
}
impl Scratch {
    pub const fn new() -> Self {
        Self {
            cells: [Cell::EMPTY; 256],
            counts: [0; 256],
        }
    }
}

struct DecodeWork<'w> {
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
    credit: &'w mut u8,
}
impl crate::decode_work::Work for DecodeWork<'_> {
    fn charge(&mut self, now: Tick, charge: Charge) -> Result<(), crate::decode_work::Error> {
        use crate::decode_work::Error as DecodeError;
        if charge.output_bytes != 0 || charge.unlinks != 0 {
            return Err(DecodeError::InvalidState);
        }
        self.budget
            .charge(self.work, now, charge.io_bytes, charge.records, self.credit)
            .map_err(DecodeError::from)
    }
}
impl From<crate::header_text::Error> for Error {
    fn from(error: crate::header_text::Error) -> Self {
        match error {
            crate::header_text::Error::Work(stop) => Self::Work(stop),
            crate::header_text::Error::InterpretationLimit => Self::InterpretationLimit,
            crate::header_text::Error::InvalidState => Self::InvalidState,
        }
    }
}
#[derive(Clone, Copy)]
enum Input<'a> {
    Utf8 { text: &'a str, position: usize },
    Header(crate::header_text::Cursor<'a>),
    Phrase(crate::header_phrase::decode::Cursor<'a>),
    Comment(crate::header_comment::decode::Cursor<'a>),
}
#[derive(Clone, Copy)]
struct Source<'a> {
    input: Input<'a>,
    pending: Option<Decomposition>,
    next: u8,
}
enum Read {
    Cell(Cell),
    Yield,
    End,
}
impl<'a> Source<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            input: Input::Utf8 { text, position: 0 },
            pending: None,
            next: 0,
        }
    }
    fn header(bytes: &'a [u8]) -> Self {
        Self {
            input: Input::Header(crate::header_text::Cursor::new(bytes)),
            pending: None,
            next: 0,
        }
    }
    fn phrase(cursor: crate::header_phrase::decode::Cursor<'a>) -> Self {
        Self {
            input: Input::Phrase(cursor),
            pending: None,
            next: 0,
        }
    }
    fn comment(proof: crate::header_comment::Validated<'a>) -> Self {
        Self {
            input: Input::Comment(proof.decode()),
            pending: None,
            next: 0,
        }
    }
    fn at(&self, other: &Self) -> bool {
        let same = match (self.input, other.input) {
            (
                Input::Utf8 { text, position },
                Input::Utf8 {
                    text: right,
                    position: at,
                },
            ) => std::ptr::eq(text, right) && position == at,
            (Input::Header(left), Input::Header(right)) => left.at(&right),
            (Input::Phrase(left), Input::Phrase(right)) => left.at(&right),
            (Input::Comment(left), Input::Comment(right)) => left.at(&right),
            _ => false,
        };
        same && self.pending == other.pending && self.next == other.next
    }
    fn is_header(&self) -> bool {
        matches!(
            self.input,
            Input::Header(_) | Input::Phrase(_) | Input::Comment(_)
        )
    }
    fn is_encoding_problem(&self) -> bool {
        match self.input {
            Input::Header(cursor) => cursor.is_encoding_problem(),
            Input::Phrase(cursor) => cursor.is_encoding_problem(),
            Input::Comment(cursor) => cursor.is_encoding_problem(),
            Input::Utf8 { .. } => false,
        }
    }
    fn read(
        &mut self,
        work: &mut Meter,
        budget: &mut HeaderBudget,
        credit: &mut u8,
        now: Tick,
    ) -> Result<Read, Error> {
        if self.pending.is_none() {
            let value = match &mut self.input {
                Input::Utf8 { text, position } => {
                    let tail = text.get(*position..).ok_or(Error::InvalidState)?;
                    let Some(first) = tail.as_bytes().first() else {
                        return Ok(Read::End);
                    };
                    let width = match first {
                        0..=0x7f => 1,
                        0x80..=0xdf => 2,
                        0xe0..=0xef => 3,
                        _ => 4,
                    };
                    budget.charge(work, now, width, 2, credit)?;
                    let value = tail.chars().next().ok_or(Error::InvalidState)?;
                    *position = position
                        .checked_add(value.len_utf8())
                        .ok_or(Error::InvalidState)?;
                    value
                }
                Input::Header(cursor) => {
                    budget.charge(work, now, 0, 1, credit)?;
                    let mut charged = DecodeWork {
                        work,
                        budget,
                        credit,
                    };
                    match cursor.poll_with_work(now, &mut charged)? {
                        crate::header_text::Status::Scalar(value) => {
                            budget.charge(work, now, 0, 1, credit)?;
                            value
                        }
                        crate::header_text::Status::Yield => return Ok(Read::Yield),
                        crate::header_text::Status::Complete => return Ok(Read::End),
                    }
                }
                Input::Phrase(cursor) => {
                    budget.charge(work, now, 0, 1, credit)?;
                    let mut charged = DecodeWork {
                        work,
                        budget,
                        credit,
                    };
                    match cursor.poll_with_work(now, &mut charged)? {
                        crate::header_phrase::decode::Status::Scalar(value) => {
                            budget.charge(work, now, 0, 1, credit)?;
                            value
                        }
                        crate::header_phrase::decode::Status::Yield => return Ok(Read::Yield),
                        crate::header_phrase::decode::Status::Complete => return Ok(Read::End),
                    }
                }
                Input::Comment(cursor) => {
                    budget.charge(work, now, 0, 1, credit)?;
                    let mut charged = DecodeWork {
                        work,
                        budget,
                        credit,
                    };
                    match cursor.poll_with_work(now, &mut charged)? {
                        crate::header_comment::decode::Status::Scalar(value) => {
                            budget.charge(work, now, 0, 1, credit)?;
                            value
                        }
                        crate::header_comment::decode::Status::Yield => return Ok(Read::Yield),
                        crate::header_comment::decode::Status::Complete => return Ok(Read::End),
                    }
                }
            };
            self.pending = Some(unicode::decompose(value).map_err(|_| Error::InvalidTable)?);
            self.next = 0;
        }
        budget.charge(work, now, 0, 1, credit)?;
        let pending = self.pending.ok_or(Error::InvalidState)?;
        let value = pending
            .iter()
            .nth(usize::from(self.next))
            .ok_or(Error::InvalidState)?;
        let class = unicode::combining_class(value).map_err(|_| Error::InvalidTable)?;
        self.next = self.next.checked_add(1).ok_or(Error::InvalidState)?;
        if usize::from(self.next) == pending.iter().len() {
            self.pending = None;
            self.next = 0;
        }
        Ok(Read::Cell(Cell { value, class }))
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Scan,
    Insert { cell: Cell, at: usize },
    Compute,
    EmitStarter,
    Emit,
    Boundary,
    Done,
}
enum Ordered {
    Cell(Cell),
    Yield,
    End,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Scalar(char),
    Yield,
    Complete,
}

/// The borrowed work meters cannot be replaced or copied by checkpoints.
pub struct Cursor<'a, 'w> {
    scratch: &'w mut Scratch,
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
    scan: Source<'a>,
    start: Source<'a>,
    end: Source<'a>,
    resume: Source<'a>,
    phase: Phase,
    initial: Option<char>,
    composed: Option<char>,
    held: Option<char>,
    boundary: Option<char>,
    last_class: u8,
    unconsumed: bool,
    used: usize,
    overflow: bool,
    classes: [u64; 4],
    ordered_index: usize,
    ordered_class: Option<u8>,
    ordered_left: u32,
    failure: Option<Error>,
    work_credit: u8,
    encoding_problem: bool,
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub fn new(
        text: &'a str,
        scratch: &'w mut Scratch,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Self {
        Self::from_source(Source::new(text), scratch, work, budget)
    }
    /// The caller must authorize an unstructured field/form before normalization.
    pub fn from_unstructured_header(
        bytes: &'a [u8],
        scratch: &'w mut Scratch,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Self {
        Self::from_source(Source::header(bytes), scratch, work, budget)
    }
    /// Supply a complete validated phrase and its exact range in the admitted
    /// field value. The caller authorizes field/form selection before this call.
    pub fn from_phrase(
        proof: crate::header_phrase::Validated<'a>,
        field: &'a [u8],
        extent: crate::header_phrase::Extent,
        scratch: &'w mut Scratch,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Result<Self, Error> {
        let cursor = crate::header_phrase::decode::Cursor::new(proof, field, extent)?;
        Ok(Self::from_source(
            Source::phrase(cursor),
            scratch,
            work,
            budget,
        ))
    }
    /// Supply the separately selected, complete fallback comment proof.
    /// The caller authorizes field/form selection before this call.
    pub fn from_comment(
        proof: crate::header_comment::Validated<'a>,
        scratch: &'w mut Scratch,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Self {
        Self::from_source(Source::comment(proof), scratch, work, budget)
    }
    fn from_source(
        source: Source<'a>,
        scratch: &'w mut Scratch,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Self {
        Self {
            scratch,
            work,
            budget,
            scan: source,
            start: source,
            end: source,
            resume: source,
            phase: Phase::Scan,
            initial: None,
            composed: None,
            held: None,
            boundary: None,
            last_class: 0,
            unconsumed: false,
            used: 0,
            overflow: false,
            classes: [0; 4],
            ordered_index: 0,
            ordered_class: None,
            ordered_left: 0,
            failure: None,
            work_credit: 0,
            encoding_problem: false,
        }
    }
    /// Final at completion; source malformation is distinct from budget refusal.
    pub const fn is_encoding_problem(&self) -> bool {
        self.encoding_problem
    }
    /// The owner must serialize the last scalar and finish its JSON frame first.
    /// Done may already hold when poll returns that final scalar.
    pub(crate) fn finish(
        self,
    ) -> Result<(&'w mut Meter, &'w mut HeaderBudget, &'w mut Scratch), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !matches!(self.phase, Phase::Done) {
            return Err(Error::InvalidState);
        }
        Ok((self.work, self.budget, self.scratch))
    }
    /// Charge actual serialized bytes, or zero for a post-turn deadline check.
    /// Refusal retires the cursor even after its final scalar was returned.
    pub fn charge_output(&mut self, now: Tick, bytes: u64) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self
            .work
            .charge(
                now,
                Charge {
                    output_bytes: bytes,
                    ..Charge::default()
                },
            )
            .map_err(Error::Work);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    /// Callers bracket turns with clock/cancellation checks and charge output.
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.phase, Phase::Done) {
            return Ok(Status::Complete);
        }
        let result = self.advance(now);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn advance(&mut self, now: Tick) -> Result<Status, Error> {
        let turns = if self.scan.is_header() { 1 } else { 32 };
        for _ in 0..turns {
            self.budget
                .charge(self.work, now, 0, 1, &mut self.work_credit)?;
            if let Some(status) = self.step(now)? {
                return Ok(status);
            }
        }
        Ok(Status::Yield)
    }
    fn step(&mut self, now: Tick) -> Result<Option<Status>, Error> {
        match self.phase {
            Phase::Scan => {
                let before = self.scan;
                let read = self
                    .scan
                    .read(self.work, self.budget, &mut self.work_credit, now)?;
                self.encoding_problem |= self.scan.is_encoding_problem();
                match read {
                    Read::Cell(cell) if cell.class == 0 => {
                        if self.initial.is_none() && self.used == 0 && !self.overflow {
                            self.initial = Some(cell.value);
                            self.start = self.scan;
                        } else {
                            self.boundary = Some(cell.value);
                            self.end = before;
                            self.resume = self.scan;
                            self.prepare()?;
                        }
                    }
                    Read::Cell(cell) => self.mark(cell)?,
                    Read::Yield => return Ok(Some(Status::Yield)),
                    Read::End => {
                        // Header EOF advances its cursor; exclude that turn from replay.
                        self.boundary = None;
                        self.end = before;
                        self.resume = self.scan;
                        self.prepare()?;
                    }
                }
            }
            Phase::Insert { cell, at } => {
                if let Some(previous) = at.checked_sub(1) {
                    let old = *self
                        .scratch
                        .cells
                        .get(previous)
                        .ok_or(Error::InvalidState)?;
                    if old.class > cell.class {
                        *self.scratch.cells.get_mut(at).ok_or(Error::InvalidState)? = old;
                        self.phase = Phase::Insert { cell, at: previous };
                        return Ok(None);
                    }
                }
                *self.scratch.cells.get_mut(at).ok_or(Error::InvalidState)? = cell;
                self.used = self.used.checked_add(1).ok_or(Error::InvalidState)?;
                self.phase = Phase::Scan;
            }
            Phase::Compute => match self.ordered(now)? {
                Ordered::Cell(cell) => {
                    if !self.absorb(cell)? {
                        self.unconsumed = true;
                    }
                }
                Ordered::Yield => {}
                Ordered::End => {
                    self.held = self.composed;
                    if self.unconsumed {
                        self.composed = self.initial;
                        self.last_class = 0;
                        self.reset_order()?;
                        self.phase = Phase::EmitStarter;
                    } else {
                        self.phase = Phase::Boundary;
                    }
                }
            },
            Phase::EmitStarter => {
                self.phase = Phase::Emit;
                if let Some(value) = self.held.take() {
                    return Ok(Some(Status::Scalar(value)));
                }
            }
            Phase::Emit => match self.ordered(now)? {
                Ordered::Cell(cell) => {
                    if !self.absorb(cell)? {
                        return Ok(Some(Status::Scalar(cell.value)));
                    }
                }
                Ordered::Yield => {}
                Ordered::End => {
                    self.held = None;
                    self.phase = Phase::Boundary;
                }
            },
            Phase::Boundary => {
                if let Some(next) = self.boundary.take() {
                    let mut output = None;
                    let starter = if let Some(old) = self.held.take() {
                        if let Some(composed) =
                            unicode::compose(old, next).map_err(|_| Error::InvalidTable)?
                        {
                            composed
                        } else {
                            output = Some(old);
                            next
                        }
                    } else {
                        next
                    };
                    self.initial = Some(starter);
                    self.used = 0;
                    self.overflow = false;
                    self.classes = [0; 4];
                    self.scan = self.resume;
                    self.start = self.resume;
                    self.phase = Phase::Scan;
                    if let Some(value) = output {
                        return Ok(Some(Status::Scalar(value)));
                    }
                } else {
                    self.phase = Phase::Done;
                    return Ok(Some(
                        self.held.take().map_or(Status::Complete, Status::Scalar),
                    ));
                }
            }
            Phase::Done => return Ok(Some(Status::Complete)),
        }
        Ok(None)
    }
    fn mark(&mut self, cell: Cell) -> Result<(), Error> {
        let class = usize::from(cell.class);
        let mask = self
            .classes
            .get_mut(class / 64)
            .ok_or(Error::InvalidState)?;
        let bit = 1u64 << (class % 64);
        let count = self
            .scratch
            .counts
            .get_mut(class)
            .ok_or(Error::InvalidState)?;
        *count = if *mask & bit == 0 {
            1
        } else {
            count.checked_add(1).ok_or(Error::InvalidState)?
        };
        *mask |= bit;
        if !self.overflow && self.used < self.scratch.cells.len() {
            self.phase = Phase::Insert {
                cell,
                at: self.used,
            };
        } else {
            self.overflow = true;
        }
        Ok(())
    }
    fn prepare(&mut self) -> Result<(), Error> {
        self.composed = self.initial;
        self.last_class = 0;
        self.unconsumed = false;
        if self.used == 0 && !self.overflow {
            self.held = self.initial;
            self.phase = Phase::Boundary;
        } else {
            self.reset_order()?;
            self.phase = if self.initial.is_none() {
                Phase::Emit
            } else {
                Phase::Compute
            };
        }
        Ok(())
    }
    fn next_class(&self, after: u16) -> Option<u8> {
        let mut word_index = usize::from(after / 64);
        let mut shift = u32::from(after % 64);
        while let Some(word) = self.classes.get(word_index) {
            let available = *word & (u64::MAX << shift);
            if available != 0 {
                return u8::try_from(word_index * 64 + available.trailing_zeros() as usize).ok();
            }
            word_index += 1;
            shift = 0;
        }
        None
    }
    fn reset_order(&mut self) -> Result<(), Error> {
        self.ordered_index = 0;
        self.ordered_class = self.next_class(1);
        self.scan = self.start;
        self.ordered_left = match self.ordered_class {
            Some(class) => *self
                .scratch
                .counts
                .get(usize::from(class))
                .ok_or(Error::InvalidState)?,
            None => 0,
        };
        Ok(())
    }
    fn ordered(&mut self, now: Tick) -> Result<Ordered, Error> {
        if !self.overflow {
            if self.ordered_index == self.used {
                return Ok(Ordered::End);
            }
            let cell = *self
                .scratch
                .cells
                .get(self.ordered_index)
                .ok_or(Error::InvalidState)?;
            self.ordered_index += 1;
            return Ok(Ordered::Cell(cell));
        }
        let Some(class) = self.ordered_class else {
            return Ok(Ordered::End);
        };
        if self.scan.at(&self.end) {
            if self.ordered_left != 0 {
                return Err(Error::InvalidState);
            }
            self.ordered_class = self.next_class(u16::from(class) + 1);
            self.scan = self.start;
            self.ordered_left = match self.ordered_class {
                Some(next) => *self
                    .scratch
                    .counts
                    .get(usize::from(next))
                    .ok_or(Error::InvalidState)?,
                None => return Ok(Ordered::End),
            };
            return Ok(Ordered::Yield);
        }
        let read = self
            .scan
            .read(self.work, self.budget, &mut self.work_credit, now)?;
        self.encoding_problem |= self.scan.is_encoding_problem();
        let cell = match read {
            Read::Cell(cell) => cell,
            Read::Yield => return Ok(Ordered::Yield),
            Read::End => return Err(Error::InvalidState),
        };
        if cell.class == class {
            self.ordered_left = self
                .ordered_left
                .checked_sub(1)
                .ok_or(Error::InvalidState)?;
            Ok(Ordered::Cell(cell))
        } else {
            Ok(Ordered::Yield)
        }
    }
    fn absorb(&mut self, cell: Cell) -> Result<bool, Error> {
        if let Some(starter) = self.composed {
            if self.last_class == 0 || self.last_class < cell.class {
                if let Some(value) =
                    unicode::compose(starter, cell.value).map_err(|_| Error::InvalidTable)?
                {
                    self.composed = Some(value);
                    return Ok(true);
                }
            }
        }
        self.last_class = cell.class;
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use crate::ports::Deadline;
    fn work() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 1000).unwrap(),
            Charge {
                io_bytes: crate::admission::WorkLimits::default().foreground_io_bytes,
                records: crate::admission::WorkLimits::default().foreground_records,
                ..Charge::default()
            },
        )
    }
    fn project(input: &str) -> (String, u64, u64) {
        let mut scratch = Scratch::new();
        let mut work = work();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(input, &mut scratch, &mut work, &mut budget);
        let mut output = String::new();
        for _ in 0..2_000_000 {
            let before = cursor.work.remaining();
            let status = cursor.poll(Tick(1)).unwrap();
            assert!(before.records - cursor.work.remaining().records <= 8);
            assert_eq!(cursor.work.remaining().output_bytes, 0);
            match status {
                Status::Scalar(value) => output.push(value),
                Status::Yield => {}
                Status::Complete => {
                    assert_eq!(cursor.poll(Tick(2000)), Ok(Status::Complete));
                    assert_eq!(
                        crate::admission::WorkLimits::default().foreground_records
                            - cursor.work.remaining().records,
                        (16_000_000 - cursor.budget.steps).div_ceil(16)
                    );
                    return (
                        output,
                        16 * 1024 * 1024 - cursor.budget.bytes,
                        16_000_000 - cursor.budget.steps,
                    );
                }
            }
        }
        panic!("normalization did not finish");
    }
    #[test]
    fn fixed_layout_and_basic_composition_boundaries() {
        assert_eq!(std::mem::size_of::<Scratch>(), 3072);
        assert!(std::mem::size_of::<Source<'_>>() <= 256);
        assert!(
            std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 1024
        );
        for (input, expected) in [
            ("", ""),
            ("abc", "abc"),
            ("e\u{301}", "é"),
            ("é", "é"),
            ("\u{212b}", "Å"),
            ("\u{1100}\u{1161}\u{11a8}", "각"),
            ("\u{0b47}\u{0b3e}", "\u{0b4b}"),
            ("\u{0b47}\u{301}\u{0b3e}", "\u{0b47}\u{301}\u{0b3e}"),
            ("a\u{315}\u{300}", "à\u{315}"),
            ("\u{315}\u{300}a", "\u{300}\u{315}a"),
            ("a\u{301}\u{301}", "á\u{301}"),
            ("a\u{352}\u{301}", "a\u{352}\u{301}"),
            (
                "\0\u{378}\u{fdd0}\u{10ffff}\u{fb01}",
                "\0\u{378}\u{fdd0}\u{10ffff}\u{fb01}",
            ),
        ] {
            assert_eq!(project(input).0, expected, "{input:?}");
        }
    }
    #[test]
    fn fast_and_replay_boundaries_preserve_long_stable_runs_and_prefix_work() {
        for count in [255, 256, 257, 1024] {
            let marks: String = ['\u{315}', '\u{300}']
                .into_iter()
                .cycle()
                .take(count)
                .collect();
            let input = format!("az{marks}b");
            let expected = format!(
                "az{}{}b",
                "\u{300}".repeat(count / 2),
                "\u{315}".repeat(count.div_ceil(2))
            );
            assert_eq!(project(&input).0, expected);
            let input = format!("a{marks}z");
            let expected = format!(
                "à{}{}z",
                "\u{300}".repeat(count / 2 - 1),
                "\u{315}".repeat(count.div_ceil(2))
            );
            assert_eq!(project(&input).0, expected);
            let leading = format!("{marks}z");
            let expected = format!(
                "{}{}z",
                "\u{300}".repeat(count / 2),
                "\u{315}".repeat(count.div_ceil(2))
            );
            assert_eq!(project(&leading).0, expected);
        }
        let prefix = "x".repeat(10000);
        let tail = format!("a{}z", "\u{315}\u{300}".repeat(300));
        let input = prefix.clone() + &tail;
        let (output, bytes, _) = project(&input);
        assert_eq!(
            output,
            prefix.clone() + "à" + &"\u{300}".repeat(299) + &"\u{315}".repeat(300) + "z"
        );
        assert_eq!(bytes, input.len() as u64 + 4 * 1200);
        let maximal = "a".repeat(1024 * 1024);
        let (output, bytes, steps) = project(&maximal);
        assert_eq!(output, maximal);
        assert_eq!(bytes, maximal.len() as u64);
        assert!(steps < 16_000_000);
    }
    #[test]
    fn replay_restores_pending_decomposition_and_equal_class_order() {
        let mut scratch = Scratch::new();
        for count in [255, 256, 257, 600] {
            let input = format!(
                "é{}é{}z",
                "\u{315}\u{344}".repeat(count),
                "\u{301}\u{300}".repeat(count)
            );
            let expected = format!(
                "é{}{}é{}z",
                "\u{308}\u{301}".repeat(count),
                "\u{315}".repeat(count),
                "\u{301}\u{300}".repeat(count)
            );
            assert_eq!(project(&input).0, expected);
            assert_eq!(project(&expected).0, expected);
        }
        // A refused aggregate must remain refused in the next projection.
        let mut budget = HeaderBudget {
            bytes: 0,
            steps: 16_000_000,
            exhausted: false,
        };
        let mut meter = work();
        assert_eq!(
            Cursor::new("a", &mut scratch, &mut meter, &mut budget).poll(Tick(1)),
            Err(Error::InterpretationLimit)
        );
        let mut meter = work();
        assert_eq!(
            Cursor::new("", &mut scratch, &mut meter, &mut budget).poll(Tick(1)),
            Err(Error::InterpretationLimit)
        );
        for (limit, reason) in [
            (
                Charge {
                    records: 100,
                    ..Charge::default()
                },
                Stop::IoBytes,
            ),
            (
                Charge {
                    io_bytes: 100,
                    ..Charge::default()
                },
                Stop::Records,
            ),
        ] {
            let mut meter = Meter::new(Deadline::after(Tick(0), 100).unwrap(), limit);
            let mut budget = HeaderBudget::new();
            let mut cursor = Cursor::new("a", &mut scratch, &mut meter, &mut budget);
            assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(reason)));
            assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(reason)));
        }
    }
    #[test]
    fn streamed_output_uses_the_same_meter_and_refusals_retire() {
        let mut scratch = Scratch::new();
        let mut budget = HeaderBudget::new();
        let mut work = Meter::new(
            Deadline::after(Tick(0), 1000).unwrap(),
            Charge {
                io_bytes: 100,
                records: 100,
                output_bytes: 3,
                ..Charge::default()
            },
        );
        let mut cursor = Cursor::new("e\u{301}x", &mut scratch, &mut work, &mut budget);
        assert_eq!(cursor.poll(Tick(1)), Ok(Status::Scalar('é')));
        cursor.charge_output(Tick(2), 2).unwrap();
        assert_eq!(cursor.work.remaining().output_bytes, 1);
        assert_eq!(cursor.poll(Tick(3)), Ok(Status::Scalar('x')));
        cursor.charge_output(Tick(4), 1).unwrap();
        assert_eq!(cursor.poll(Tick(5)), Ok(Status::Complete));
        assert_eq!(
            cursor.charge_output(Tick(6), 1),
            Err(Error::Work(Stop::OutputBytes))
        );
        assert_eq!(cursor.poll(Tick(6)), Err(Error::Work(Stop::OutputBytes)));
        assert_eq!(
            cursor.charge_output(Tick(6), 0),
            Err(Error::Work(Stop::OutputBytes))
        );
        let mut work = self::work();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new("a", &mut scratch, &mut work, &mut budget);
        assert_eq!(cursor.poll(Tick(1)), Ok(Status::Scalar('a')));
        assert_eq!(
            cursor.charge_output(Tick(1000), 0),
            Err(Error::Work(Stop::Deadline))
        );
        assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
    }
    #[test]
    fn budgets_and_deadlines_retire_during_scanning_or_replay_and_scratch_reuses() {
        let input = format!("a{}b", "\u{315}\u{300}".repeat(300));
        let mut scratch = Scratch::new();
        for phase in 0..4 {
            let mut work = work();
            let mut budget = HeaderBudget::new();
            let mut cursor = Cursor::new(&input, &mut scratch, &mut work, &mut budget);
            let mut reached = false;
            for _ in 0..10000 {
                reached = match phase {
                    0 => {
                        matches!(cursor.phase, Phase::Scan)
                            && matches!(cursor.scan.input, Input::Utf8 { position, .. } if position > 0)
                    }
                    1 => matches!(cursor.phase, Phase::Insert { .. }),
                    2 => cursor.overflow && matches!(cursor.phase, Phase::Compute),
                    3 => cursor.overflow && matches!(cursor.phase, Phase::Emit),
                    _ => false,
                };
                if reached {
                    break;
                }
                assert_ne!(cursor.poll(Tick(1)).unwrap(), Status::Complete);
            }
            assert!(reached);
            let before = cursor.work.remaining();
            let header_before = (cursor.budget.bytes, cursor.budget.steps);
            assert_eq!(cursor.poll(Tick(1000)), Err(Error::Work(Stop::Deadline)));
            assert_eq!(cursor.work.remaining(), before);
            assert_eq!((cursor.budget.bytes, cursor.budget.steps), header_before);
            *cursor.work = self::work();
            assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
            assert_eq!(cursor.work.remaining(), self::work().remaining());
        }
        for (bytes, steps) in [
            (0, 16_000_000),
            (16 * 1024 * 1024, 0),
            (16 * 1024 * 1024, 10000),
        ] {
            let mut work = work();
            let mut budget = HeaderBudget {
                bytes,
                steps,
                exhausted: false,
            };
            let mut cursor = Cursor::new(&input, &mut scratch, &mut work, &mut budget);
            loop {
                match cursor.poll(Tick(1)) {
                    Err(Error::InterpretationLimit) => break,
                    Ok(Status::Complete) => panic!("limit did not stop work"),
                    Ok(_) => {}
                    Err(other) => panic!("unexpected {other:?}"),
                }
            }
            assert!(cursor.budget.exhausted);
            *cursor.work = self::work();
            assert_eq!(cursor.poll(Tick(1)), Err(Error::InterpretationLimit));
        }
        let mut work = work();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new("e\u{301}", &mut scratch, &mut work, &mut budget);
        assert_eq!(cursor.poll(Tick(1)), Ok(Status::Scalar('é')));
        assert_eq!(cursor.poll(Tick(1)), Ok(Status::Complete));
    }
    fn comment_proof(input: &[u8]) -> crate::header_comment::Validated<'_> {
        let mut parser = crate::header_comment::Cursor::new(input);
        let mut meter = work();
        while parser.poll(Tick(1), &mut meter).unwrap() != crate::header_comment::Status::Complete {
        }
        parser.into_validated().unwrap()
    }
    fn comment(input: &[u8]) -> (String, bool, u64) {
        let proof = comment_proof(input);
        let mut scratch = Scratch::new();
        let mut meter = work();
        let before = meter.remaining();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::from_comment(proof, &mut scratch, &mut meter, &mut budget);
        assert!(std::mem::size_of::<Source<'_>>() <= 256);
        assert!(std::mem::size_of_val(&cursor) + std::mem::size_of::<HeaderBudget>() <= 1024);
        let mut text = String::new();
        for _ in 0..20_000_000 {
            let steps = cursor.budget.steps_remaining();
            let records = cursor.work.remaining().records;
            let status = cursor.poll(Tick(1)).unwrap();
            assert!(steps - cursor.budget.steps_remaining() <= 231);
            assert!(records - cursor.work.remaining().records <= 15);
            match status {
                Status::Yield => {}
                Status::Scalar(value) => text.push(value),
                Status::Complete => {
                    let problem = cursor.is_encoding_problem();
                    let visits = before.io_bytes - meter.remaining().io_bytes;
                    assert_eq!(visits, 16 * 1024 * 1024 - budget.source_bytes_remaining());
                    return (text, problem, visits);
                }
            }
        }
        panic!("comment NFC did not finish");
    }
    #[test]
    fn comment_names_normalize_after_unquoting_placement_and_filtering() {
        for (source, expected, problem) in [
            ("()", "", false),
            ("( e\u{301} )", "é", false),
            ("(=?utf-8?q?e?= =?utf-8?q?=CC=81?=)", "é", false),
            ("(=?utf-8?q?e=00=CC=81?=)", "é", false),
            ("(e\\\0\u{301})", "é", false),
            ("((=?utf-8?q?e=CC=81?=))", "(é)", false),
            ("(=?utf-8?q?=E1=84=80?= =?utf-8?q?=E1=85=A1?=)", "가", false),
            ("(e\u{301}\u{fdd0})", "é�", true),
            ("(=?utf-8?q?=FF?=)", "�", true),
        ] {
            let (text, diagnostic, _) = comment(source.as_bytes());
            assert_eq!(
                (text.as_str(), diagnostic),
                (expected, problem),
                "{source:?}"
            );
        }
        // A maximum-width original token stresses the recognition quantum.
        let token = format!("=?utf-8?q?{}?=", "a".repeat(63));
        assert_eq!(token.len(), 75);
        let source = format!("(x {token} e\u{301})");
        assert_eq!(
            comment(source.as_bytes()).0,
            format!("x {} é", "a".repeat(63))
        );
    }
    #[test]
    fn comment_overflow_replays_checkpoints_without_rescanning_prefix() {
        let tail = format!(
            "=?utf-8?q?=C3=A9?={} =?utf-8?q?z?=",
            " =?utf-8?q?=CC=95=CD=84?=".repeat(257)
        );
        let source = format!("({tail})");
        let (text, problem, visits) = comment(source.as_bytes());
        assert!(!problem);
        assert_eq!(
            text,
            format!(
                "é{}{}z",
                "\u{308}\u{301}".repeat(257),
                "\u{315}".repeat(257)
            )
        );
        let prefixed = format!("({} {tail})", "x".repeat(10_000));
        let (long, _, long_visits) = comment(prefixed.as_bytes());
        assert_eq!(long, format!("{} {text}", "x".repeat(10_000)));
        assert!(long_visits - visits < 50_100);
    }
    #[test]
    fn comment_aggregate_and_progressed_refusals_latch() {
        let proof = comment_proof(b"(a =?utf-8?q?b?=)");
        for (bytes, steps) in [(0, 16_000_000), (16 * 1024 * 1024, 0)] {
            let mut scratch = Scratch::new();
            let mut meter = work();
            let mut budget = HeaderBudget {
                bytes,
                steps,
                exhausted: false,
            };
            let mut cursor = Cursor::from_comment(proof, &mut scratch, &mut meter, &mut budget);
            for _ in 0..100 {
                if cursor.poll(Tick(1)) == Err(Error::InterpretationLimit) {
                    break;
                }
            }
            assert_eq!(cursor.poll(Tick(1)), Err(Error::InterpretationLimit));
        }
        let mut scratch = Scratch::new();
        let mut meter = work();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::from_comment(proof, &mut scratch, &mut meter, &mut budget);
        for _ in 0..10 {
            cursor.poll(Tick(1)).unwrap();
        }
        assert_eq!(cursor.poll(Tick(1000)), Err(Error::Work(Stop::Deadline)));
        assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
        let mut meter = work();
        let mut cursor = Cursor::from_comment(proof, &mut scratch, &mut meter, &mut budget);
        while cursor.poll(Tick(1)).unwrap() != Status::Complete {}
        assert_eq!(
            cursor.charge_output(Tick(1), 1),
            Err(Error::Work(Stop::OutputBytes))
        );
        assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::OutputBytes)));
    }
    fn phrase_proof(input: &[u8]) -> crate::header_phrase::Validated<'_> {
        let mut parser = crate::header_phrase::Cursor::new(input);
        let mut work = work();
        while !matches!(
            parser.poll(Tick(1), &mut work).unwrap(),
            crate::header_phrase::Status::Complete(_)
        ) {}
        parser.into_validated().unwrap()
    }
    fn phrase_trace(input: &[u8]) -> (String, bool, u64, u64) {
        let proof = phrase_proof(input);
        let mut scratch = Scratch::new();
        let mut work = Meter::new(
            Deadline::after(Tick(0), 1000).unwrap(),
            Charge {
                io_bytes: crate::admission::WorkLimits::default().foreground_io_bytes,
                records: crate::admission::WorkLimits::default().foreground_records,
                output_bytes: 32 * 1024 * 1024,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let before = work.remaining();
        let mut cursor = Cursor::from_phrase(
            proof,
            input,
            crate::header_phrase::Extent {
                start: 0,
                end: input.len(),
            },
            &mut scratch,
            &mut work,
            &mut budget,
        )
        .unwrap();
        assert!(std::mem::size_of::<Source<'_>>() <= 256);
        assert!(
            std::mem::size_of_val(&cursor) + std::mem::size_of::<HeaderBudget>() <= 1024,
            "NFC cursor + budget {}",
            std::mem::size_of_val(&cursor) + std::mem::size_of::<HeaderBudget>()
        );
        let mut text = String::new();
        let mut peak_steps = 0;
        for _ in 0..20_000_000 {
            let steps = cursor.budget.steps_remaining();
            let records = cursor.work.remaining().records;
            let status = cursor.poll(Tick(1)).unwrap();
            peak_steps = peak_steps.max(steps - cursor.budget.steps_remaining());
            assert!(peak_steps <= 231);
            assert!(records - cursor.work.remaining().records <= 15);
            match status {
                Status::Yield => {}
                Status::Scalar(value) => {
                    cursor
                        .charge_output(Tick(1), value.len_utf8() as u64)
                        .unwrap();
                    text.push(value);
                }
                Status::Complete => {
                    let problem = cursor.is_encoding_problem();
                    let visits = before.io_bytes - work.remaining().io_bytes;
                    assert_eq!(visits, 16 * 1024 * 1024 - budget.source_bytes_remaining());
                    return (text, problem, visits, peak_steps);
                }
            }
        }
        panic!("phrase NFC did not finish");
    }
    fn phrase(input: &[u8]) -> (String, bool, u64) {
        let (text, problem, visits, _) = phrase_trace(input);
        (text, problem, visits)
    }
    #[test]
    fn phrase_names_normalize_after_placement_unquoting_and_filtering() {
        for (source, expected, problem) in [
            (" \" e\u{301} \" ", "é", false),
            ("=?utf-8?q?e?= =?utf-8?q?=CC=81?=", "é", false),
            ("=?utf-8?q?e=00=CC=81?=", "é", false),
            ("\"e\"\u{301}", "é", false),
            ("=?utf-8?q?=E1=84=80?= =?utf-8?q?=E1=85=A1?=", "가", false),
            ("\"\\\0e\u{301}\u{fdd0}\"", "é�", true),
            ("=?utf-8?q?=FF?=", "�", true),
            ("=?utf-8?q?e?= (x) =?utf-8?q?=CC=81?=", "e \u{301}", false),
        ] {
            let (text, diagnostic, _) = phrase(source.as_bytes());
            assert_eq!(
                (text, diagnostic),
                (expected.to_owned(), problem),
                "{source}"
            );
        }
    }
    #[test]
    fn phrase_recognition_with_a_gap_covers_the_maximal_poll() {
        let source = format!("x {}", "a".repeat(75));
        let (text, _, _, peak) = phrase_trace(source.as_bytes());
        assert_eq!(text, source);
        assert_eq!(peak, 231);
    }
    #[test]
    fn phrase_pending_decomposition_is_actually_restored() {
        let source = format!(
            "=?utf-8?q?=C3=A9?={} =?utf-8?q?=C3=A9?={} =?utf-8?q?z?=",
            " =?utf-8?q?=CC=95=CD=84?=".repeat(257),
            " =?utf-8?q?=CC=81=CC=80?=".repeat(257)
        );
        let expected = format!(
            "é{}{}é{}z",
            "\u{308}\u{301}".repeat(257),
            "\u{315}".repeat(257),
            "\u{301}\u{300}".repeat(257)
        );
        let proof = phrase_proof(source.as_bytes());
        let mut scratch = Scratch::new();
        let mut meter = work();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::from_phrase(
            proof,
            source.as_bytes(),
            crate::header_phrase::Extent {
                start: 0,
                end: source.len(),
            },
            &mut scratch,
            &mut meter,
            &mut budget,
        )
        .unwrap();
        let mut text = String::new();
        let mut start_restored = false;
        let mut resume_restored = false;
        for _ in 0..2_000_000 {
            start_restored |= cursor.overflow
                && matches!(cursor.phase, Phase::Compute | Phase::Emit)
                && cursor.scan.at(&cursor.start)
                && cursor.scan.pending.is_some();
            resume_restored |= matches!(cursor.phase, Phase::Scan)
                && cursor.scan.at(&cursor.resume)
                && cursor.scan.pending.is_some();
            match cursor.poll(Tick(1)).unwrap() {
                Status::Yield => {}
                Status::Scalar(value) => text.push(value),
                Status::Complete => {
                    assert_eq!(text, expected);
                    assert!(start_restored && resume_restored);
                    return;
                }
            }
        }
        panic!("pending decomposition replay did not finish");
    }
    #[test]
    fn phrase_overflow_restores_word_and_pending_decomposition_checkpoints() {
        let word = "=?utf-8?q?=CC=95=CC=80=CC=95=CC=80=CC=95=CC=80=CC=95=CC=80=CC=95=CC=80?=";
        let source = format!("=?utf-8?q?a?={}", format!(" {word}").repeat(30));
        assert_eq!(
            phrase(source.as_bytes()).0,
            format!("à{}{}", "\u{300}".repeat(149), "\u{315}".repeat(150))
        );
        let source = format!("=?utf-8?q?a?={}", " =?utf-8?q?=CD=84?=".repeat(257));
        assert_eq!(
            phrase(source.as_bytes()).0,
            format!("ä\u{301}{}", "\u{308}\u{301}".repeat(256))
        );
        let source = format!("\" a{} \"", "\u{315}\u{300}".repeat(150));
        assert_eq!(
            phrase(source.as_bytes()).0,
            format!("à{}{}", "\u{300}".repeat(149), "\u{315}".repeat(150))
        );
    }
    #[test]
    fn phrase_replay_never_rescans_prefix_and_maximal_ascii_fits() {
        let tail = format!("=?utf-8?q?a?={}", " =?utf-8?q?=CC=95=CC=80?=".repeat(150));
        let (_, _, short) = phrase(format!("x {tail}").as_bytes());
        let (_, _, long) = phrase(format!("x{} {tail}", "x".repeat(10_000)).as_bytes());
        assert_eq!(long - short, 40_000);
        let input = "a".repeat(1024 * 1024);
        let (output, problem, visits) = phrase(input.as_bytes());
        assert_eq!(output, input);
        assert!(!problem);
        assert_eq!(visits, 4 * 1024 * 1024 + 2);
    }
    #[test]
    fn phrase_context_and_terminal_failures_survive_normalization() {
        let field = b"=?utf-8?q?e=CC=81?=<a@b>";
        let length = field.len() - b"<a@b>".len();
        let proof = phrase_proof(field.get(..length).unwrap());
        let mut scratch = Scratch::new();
        let mut meter = work();
        let mut budget = HeaderBudget::new();
        assert!(matches!(
            Cursor::from_phrase(
                proof,
                field,
                crate::header_phrase::Extent {
                    start: 1,
                    end: length
                },
                &mut scratch,
                &mut meter,
                &mut budget
            ),
            Err(Error::InvalidState)
        ));
        let mut cursor = Cursor::from_phrase(
            proof,
            field,
            crate::header_phrase::Extent {
                start: 0,
                end: length,
            },
            &mut scratch,
            &mut meter,
            &mut budget,
        )
        .unwrap();
        let mut text = String::new();
        loop {
            match cursor.poll(Tick(1)).unwrap() {
                Status::Yield => {}
                Status::Scalar(value) => text.push(value),
                Status::Complete => break,
            }
        }
        assert_eq!(text.as_bytes(), field.get(..length).unwrap());
        assert_eq!(
            cursor.charge_output(Tick(1), 1),
            Err(Error::Work(Stop::OutputBytes))
        );
        assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::OutputBytes)));
        let input = b"name more";
        let proof = phrase_proof(input);
        for deadline in [false, true] {
            let mut meter = work();
            let mut budget = HeaderBudget::new();
            let mut cursor = Cursor::from_phrase(
                proof,
                input,
                crate::header_phrase::Extent {
                    start: 0,
                    end: input.len(),
                },
                &mut scratch,
                &mut meter,
                &mut budget,
            )
            .unwrap();
            while !matches!(cursor.poll(Tick(1)).unwrap(), Status::Scalar(_)) {}
            let expected = if deadline {
                Error::Work(Stop::Deadline)
            } else {
                cursor.budget.steps = 0;
                Error::InterpretationLimit
            };
            assert_eq!(
                cursor.poll(if deadline { Tick(1000) } else { Tick(1) }),
                Err(expected)
            );
            *cursor.work = work();
            assert_eq!(cursor.poll(Tick(1)), Err(expected));
        }
        let mut meter = work();
        let mut budget = HeaderBudget::new();
        budget.bytes = 0;
        let mut cursor = Cursor::from_phrase(
            proof,
            input,
            crate::header_phrase::Extent {
                start: 0,
                end: input.len(),
            },
            &mut scratch,
            &mut meter,
            &mut budget,
        )
        .unwrap();
        assert_eq!(cursor.poll(Tick(1)), Err(Error::InterpretationLimit));
        let mut fresh = work();
        let mut next = Cursor::from_phrase(
            proof,
            input,
            crate::header_phrase::Extent {
                start: 0,
                end: input.len(),
            },
            &mut scratch,
            &mut fresh,
            &mut budget,
        )
        .unwrap();
        assert_eq!(next.poll(Tick(1)), Err(Error::InterpretationLimit));
    }
    fn header(input: &[u8]) -> (String, bool, u64) {
        let mut scratch = Scratch::new();
        let mut work = Meter::new(
            Deadline::after(Tick(0), 1000).unwrap(),
            Charge {
                io_bytes: crate::admission::WorkLimits::default().foreground_io_bytes,
                records: crate::admission::WorkLimits::default().foreground_records,
                output_bytes: 32 * 1024 * 1024,
                ..Charge::default()
            },
        );
        let before = work.remaining();
        let mut budget = HeaderBudget::new();
        let mut cursor =
            Cursor::from_unstructured_header(input, &mut scratch, &mut work, &mut budget);
        assert!(std::mem::size_of_val(&cursor) + std::mem::size_of::<HeaderBudget>() <= 1024);
        assert!(std::mem::size_of::<Source<'_>>() <= 256);
        let mut output = String::new();
        for _ in 0..1_000_000 {
            let steps = cursor.budget.steps_remaining();
            let records = cursor.work.remaining().records;
            let status = cursor.poll(Tick(1)).unwrap();
            assert!(steps - cursor.budget.steps_remaining() <= 228);
            assert!(records - cursor.work.remaining().records <= 15);
            match status {
                Status::Scalar(value) => {
                    cursor
                        .charge_output(Tick(1), value.len_utf8() as u64)
                        .unwrap();
                    output.push(value);
                }
                Status::Yield => {}
                Status::Complete => {
                    let problem = cursor.is_encoding_problem();
                    assert_eq!(
                        before.io_bytes - work.remaining().io_bytes,
                        16 * 1024 * 1024 - budget.source_bytes_remaining()
                    );
                    return (output, problem, before.io_bytes - work.remaining().io_bytes);
                }
            }
        }
        panic!("header normalization did not finish");
    }
    #[test]
    fn header_normalization_crosses_words_and_filters_before_nfc() {
        for (input, expected, problem) in [
            (
                b"  =?utf-8?Q?e?=\r\n =?utf-8?Q?=CC=81?=".as_slice(),
                "é",
                false,
            ),
            (b" \t=?utf-8?Q?e=CC=81?=  ", "\té  ", false),
            (
                b"=?utf-8?Q?=E1=84=80?= =?utf-8?Q?=E1=85=A1?= =?utf-8?Q?=E1=86=A8?=",
                "각",
                false,
            ),
            (b"=?utf-8?Q?e=00=CC=81?=", "é", false),
            (b"e\0\xcc\x81\xef\xbf\xbf", "é�", true),
            (b"=?utf-8?Q?=E2=82?= =?utf-8?Q?=AC?=", "��", true),
            (b"=?utf-8?Q?e?= \r\n\tbad", "e \tbad", false),
            (b"e\xcc\x81", "é", false),
        ] {
            let (actual, diagnostic, _) = header(input);
            assert_eq!(
                (actual, diagnostic),
                (expected.to_owned(), problem),
                "{input:?}"
            );
        }
    }
    #[test]
    fn header_replay_restores_word_and_decomposition_positions_without_prefix_scans() {
        let left = vec![b'a'];
        let right = vec![b'a'];
        let original = Source::header(&left);
        let mut advanced = original;
        assert!(original.at(&advanced));
        assert!(!original.at(&Source::header(&right)));
        assert!(matches!(
            advanced
                .read(&mut work(), &mut HeaderBudget::new(), &mut 0, Tick(1))
                .unwrap(),
            Read::Yield
        ));
        assert!(!original.at(&advanced));
        let word = "=?utf-8?Q?=CC=95=CC=80=CC=95=CC=80=CC=95=CC=80=CC=95=CC=80=CC=95=CC=80?=";
        let tail = format!("=?utf-8?Q?a?={}", format!(" {word}").repeat(30));
        let expected = format!("à{}{}", "\u{300}".repeat(149), "\u{315}".repeat(150));
        let (output, problem, visits) = header(tail.as_bytes());
        assert_eq!(output, expected);
        assert!(!problem);
        for (suffix, decoded) in [
            (" b", " b"),
            (" =?utf-8?Q?=E2=82=AC?=", "€"),
            ("  =?bogus?=x", "  =?bogus?=x"),
        ] {
            let (actual, problem, _) = header(format!("{tail}{suffix}").as_bytes());
            assert_eq!(actual, format!("{expected}{decoded}"));
            assert!(!problem);
        }
        let prefix = "x".repeat(10000);
        let (output, problem, with_prefix) = header(format!("{prefix} {tail}").as_bytes());
        assert_eq!(output, format!("{prefix} {expected}"));
        assert!(!problem);
        assert_eq!(with_prefix - visits, 20002);
        let tail = format!("=?utf-8?Q?a?={}", " =?utf-8?Q?=CD=84?=".repeat(257));
        let (output, problem, _) = header(tail.as_bytes());
        assert_eq!(output, format!("ä\u{301}{}", "\u{308}\u{301}".repeat(256)));
        assert!(!problem);
    }
    #[test]
    fn header_global_limits_retire_decoding_and_maximal_ascii_fits() {
        let mut scratch = Scratch::new();
        let mut budget = HeaderBudget::new();
        budget.bytes = 2;
        let mut work = work();
        let mut cursor = Cursor::from_unstructured_header(
            b"=?utf-8?Q?a?=",
            &mut scratch,
            &mut work,
            &mut budget,
        );
        assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
        assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
        assert_eq!(cursor.poll(Tick(1)), Err(Error::InterpretationLimit));
        assert_eq!(cursor.poll(Tick(1)), Err(Error::InterpretationLimit));
        let mut fresh = self::work();
        let mut next =
            Cursor::from_unstructured_header(b"x", &mut scratch, &mut fresh, &mut budget);
        assert_eq!(next.poll(Tick(1)), Err(Error::InterpretationLimit));
        for capacity in [66, 70] {
            let mut budget = HeaderBudget::new();
            budget.bytes = capacity;
            let mut fresh = self::work();
            let before = fresh.remaining().io_bytes;
            let mut cursor = Cursor::from_unstructured_header(
                b"=?utf-8?B?4oKs?=",
                &mut scratch,
                &mut fresh,
                &mut budget,
            );
            let mut failed = false;
            for _ in 0..100 {
                match cursor.poll(Tick(1)) {
                    Err(error) => {
                        assert_eq!(error, Error::InterpretationLimit);
                        failed = true;
                        break;
                    }
                    Ok(Status::Yield) => {}
                    Ok(_) => panic!("transfer/charset exceeded aggregate bytes"),
                }
            }
            assert!(failed);
            assert_eq!(budget.source_bytes_remaining(), 0);
            assert_eq!(before - fresh.remaining().io_bytes, capacity);
        }
        let input = format!("=?utf-8?Q?{}?=", "a".repeat(63));
        assert_eq!(input.len(), 75);
        let mut budget = HeaderBudget::new();
        budget.steps = 200;
        let mut fresh = self::work();
        let mut cursor = Cursor::from_unstructured_header(
            input.as_bytes(),
            &mut scratch,
            &mut fresh,
            &mut budget,
        );
        let mut failed = false;
        for _ in 0..300 {
            match cursor.poll(Tick(1)) {
                Err(error) => {
                    assert_eq!(error, Error::InterpretationLimit);
                    failed = true;
                    break;
                }
                Ok(Status::Yield) => {}
                Ok(_) => panic!("recognition exceeded its aggregate steps"),
            }
        }
        assert!(failed);
        assert_eq!(16 * 1024 * 1024 - budget.source_bytes_remaining(), 77);
        let bytes = vec![b'x'; 1024 * 1024];
        let mut budget = HeaderBudget::new();
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 256 * 1024 * 1024,
                records: 2_000_000,
                output_bytes: 32 * 1024 * 1024,
                ..Charge::default()
            },
        );
        let mut cursor =
            Cursor::from_unstructured_header(&bytes, &mut scratch, &mut work, &mut budget);
        let mut count = 0;
        let mut done = false;
        for _ in 0..16_000_000 {
            match cursor.poll(Tick(1)).unwrap() {
                Status::Scalar('x') => count += 1,
                Status::Scalar(_) => panic!("unexpected header scalar"),
                Status::Yield => {}
                Status::Complete => {
                    done = true;
                    break;
                }
            }
        }
        assert!(done);
        assert_eq!(count, bytes.len());
        assert!(!cursor.is_encoding_problem());
        assert_eq!(
            16 * 1024 * 1024 - budget.source_bytes_remaining(),
            2 * bytes.len() as u64 + 1
        );
        assert!(work.remaining().records > 0);
    }
    #[test]
    fn header_adapter_job_refusal_does_not_retire_aggregate_budget() {
        let input = format!("=?utf-8?Q?{}?=", "a".repeat(63));
        let mut scratch = Scratch::new();
        let mut budget = HeaderBudget::new();
        let mut limited = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 1000,
                records: 16,
                ..Charge::default()
            },
        );
        let mut cursor = Cursor::from_unstructured_header(
            input.as_bytes(),
            &mut scratch,
            &mut limited,
            &mut budget,
        );
        let mut failed = false;
        for _ in 0..300 {
            match cursor.poll(Tick(1)) {
                Ok(Status::Yield) => {}
                Err(error) => {
                    assert_eq!(error, Error::Work(Stop::Records));
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    failed = true;
                    break;
                }
                Ok(_) => panic!("recognizer exceeded job records"),
            }
        }
        assert!(failed);
        // Candidate scans finish; the next 225-byte recognizer precharge refuses.
        assert_eq!(16 * 1024 * 1024 - budget.source_bytes_remaining(), 77);
        let mut fresh = work();
        let mut cursor =
            Cursor::from_unstructured_header(b"x", &mut scratch, &mut fresh, &mut budget);
        let mut output = String::new();
        let mut complete = false;
        for _ in 0..100 {
            match cursor.poll(Tick(1)).unwrap() {
                Status::Scalar(value) => output.push(value),
                Status::Yield => {}
                Status::Complete => {
                    complete = true;
                    break;
                }
            }
        }
        assert!(complete);
        assert_eq!(output, "x");
    }
    #[test]
    fn header_deadlines_retire_scanning_and_both_replay_passes() {
        let input = format!("=?utf-8?Q?a?={}", " =?utf-8?Q?=CC=95=CC=80?=".repeat(150));
        for target in 0..3 {
            let mut scratch = Scratch::new();
            let mut work = work();
            let mut budget = HeaderBudget::new();
            let mut cursor = Cursor::from_unstructured_header(
                input.as_bytes(),
                &mut scratch,
                &mut work,
                &mut budget,
            );
            let mut reached = false;
            for _ in 0..1_000_000 {
                let ready = match target {
                    0 => {
                        matches!(cursor.phase, Phase::Scan)
                            && cursor.budget.steps_remaining() < 15_999_980
                    }
                    1 => matches!(cursor.phase, Phase::Compute) && cursor.overflow,
                    _ => matches!(cursor.phase, Phase::Emit) && cursor.overflow,
                };
                if ready {
                    let bytes = cursor.budget.source_bytes_remaining();
                    let steps = cursor.budget.steps_remaining();
                    assert_eq!(cursor.poll(Tick(1000)), Err(Error::Work(Stop::Deadline)));
                    assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
                    assert_eq!(cursor.budget.source_bytes_remaining(), bytes);
                    assert_eq!(cursor.budget.steps_remaining(), steps);
                    reached = true;
                    break;
                }
                assert_ne!(cursor.poll(Tick(1)).unwrap(), Status::Complete);
            }
            assert!(reached);
        }
    }
}
