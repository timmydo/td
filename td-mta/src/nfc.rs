//! Bounded NFC over resident UTF-8; decoded-header integration is separate.
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
    fn charge(
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
        let records = u64::from(steps > u64::from(*credit));
        let next_credit = u64::from(*credit)
            .checked_add(records * 16)
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

#[derive(Clone, Copy)]
struct Source<'a> {
    text: &'a str,
    position: usize,
    pending: Option<Decomposition>,
    next: u8,
}
impl<'a> Source<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            position: 0,
            pending: None,
            next: 0,
        }
    }
    fn at(&self, other: &Self) -> bool {
        std::ptr::eq(self.text, other.text)
            && self.position == other.position
            && self.pending == other.pending
            && self.next == other.next
    }
    fn read(
        &mut self,
        work: &mut Meter,
        budget: &mut HeaderBudget,
        credit: &mut u8,
        now: Tick,
    ) -> Result<Option<Cell>, Error> {
        if self.pending.is_none() {
            let text = self.text.get(self.position..).ok_or(Error::InvalidState)?;
            let Some(first) = text.as_bytes().first() else {
                return Ok(None);
            };
            // Valid str guarantees a scalar boundary and its encoded width.
            let width = match first {
                0..=0x7f => 1,
                0x80..=0xdf => 2,
                0xe0..=0xef => 3,
                _ => 4,
            };
            budget.charge(work, now, width, 2, credit)?;
            let value = text.chars().next().ok_or(Error::InvalidState)?;
            self.pending = Some(unicode::decompose(value).map_err(|_| Error::InvalidTable)?);
            self.position = self
                .position
                .checked_add(value.len_utf8())
                .ok_or(Error::InvalidState)?;
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
        Ok(Some(Cell { value, class }))
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
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub fn new(
        text: &'a str,
        scratch: &'w mut Scratch,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Self {
        let source = Source::new(text);
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
        }
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
        for _ in 0..32 {
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
                match self
                    .scan
                    .read(self.work, self.budget, &mut self.work_credit, now)?
                {
                    Some(cell) if cell.class == 0 => {
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
                    Some(cell) => self.mark(cell)?,
                    None => {
                        self.boundary = None;
                        self.end = self.scan;
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
        let cell = self
            .scan
            .read(self.work, self.budget, &mut self.work_credit, now)?
            .ok_or(Error::InvalidState)?;
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
        assert!(std::mem::size_of::<Source<'_>>() <= 64);
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
                    0 => matches!(cursor.phase, Phase::Scan) && cursor.scan.position > 0,
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
}
