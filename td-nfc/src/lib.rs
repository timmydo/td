//! Bounded canonical composition; Unicode, source and admission policy are caller owned.
#![forbid(unsafe_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error<E> {
    Source(E),
    InvalidState,
}
impl<E: std::fmt::Display> std::fmt::Display for Error<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Source(error) => write!(f, "normalization source/admission: {error}"),
            Self::InvalidState => f.write_str("invalid normalization state"),
        }
    }
}
impl<E: std::error::Error + 'static> std::error::Error for Error<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::InvalidState => None,
        }
    }
}
/// Caller-owned live admission; called before each fixed engine transition.
/// Implementations bind their current clock/cancellation and original budgets.
/// The engine neither retains this context nor copies it into checkpoints.
pub trait Admission {
    type Error: Copy;
    fn step(&mut self) -> Result<(), Self::Error>;
}
/// A deterministic, quota-free source checkpoint.
/// Copies must retain only pure source/decoder/decomposition state, never an
/// allowance, live admission or mutable output owner. `at` compares exact
/// replay position including pending scalar/decomposition state and identity.
/// `compose` supplies canonical composition under the caller's fixed Unicode
/// tables. `turns` is a fixed quantum in 1..=32, not an admission replacement.
/// `at`, `compose` and `is_encoding_problem` perform fixed-bounded work
/// covered by the enclosing transition's Admission::step. `turns` runs once
/// per active poll before admission; it must return the same fixed quantum
/// for every checkpoint without source access. Only Reader receives context.
pub trait Source: Copy {
    type Error: Copy;
    fn at(&self, other: &Self) -> bool;
    fn compose(left: char, right: char) -> Result<Option<char>, Self::Error>;
    fn is_encoding_problem(&self) -> bool;
    fn turns(&self) -> u8;
}
/// Deterministically replay canonically decomposed/classified scalar input.
/// The caller admits every read and bounded Unicode lookup through the original
/// context. Every Cell/Yield/End must reproduce its event and next checkpoint.
/// This conditional source contract is not validated by the composition engine.
pub trait Reader<A: Admission<Error = Self::Error>>: Source {
    fn read(&mut self, admission: &mut A) -> Result<Read, Self::Error>;
}
pub enum Read {
    Cell(Cell),
    Yield,
    End,
}
#[doc(hidden)]
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Scan,
    Insert,
    Compute,
    Emit,
    Other,
}
#[doc(hidden)]
pub struct Inspection<'s, S> {
    pub scan: &'s S,
    pub start: &'s S,
    pub end: &'s S,
    pub resume: &'s S,
    pub mode: Mode,
    pub overflow: bool,
}
#[derive(Clone, Copy)]
/// One canonically decomposed scalar with its caller-supplied combining class.
/// Zero denotes a starter; this passive cell carries no validity proof.
pub struct Cell {
    pub value: char,
    pub class: u8,
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

/// One exclusive normalization owner over caller-owned source and scratch.
/// Scalar events remain provisional until complete source processing and fresh
/// enclosing admission. Cached Complete is inert; `check` latches live refusal.
/// Live state cannot duplicate the exclusive scratch borrow.
/// ```compile_fail
/// fn require_copy<T: Copy>() {}
/// fn copy_cursor<'w, S: td_nfc::Source>() {
///     require_copy::<td_nfc::Cursor<'w, S>>();
/// }
/// ```
/// ```compile_fail
/// fn require_clone<T: Clone>() {}
/// fn clone_cursor<'w, S: td_nfc::Source>() {
///     require_clone::<td_nfc::Cursor<'w, S>>();
/// }
/// ```
pub struct Cursor<'w, S: Source> {
    scratch: &'w mut Scratch,
    scan: S,
    start: S,
    end: S,
    resume: S,
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
    failure: Option<Error<S::Error>>,
    encoding_problem: bool,
}
impl<'w, S: Source> Cursor<'w, S> {
    pub fn new(source: S, scratch: &'w mut Scratch) -> Self {
        Self {
            scratch,
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
            encoding_problem: false,
        }
    }
    /// Final at completion; source malformation is distinct from budget refusal.
    pub const fn is_encoding_problem(&self) -> bool {
        self.encoding_problem
    }
    /// Passive borrowed checkpoint diagnostics, absent after any refusal.
    /// Inspection grants neither a restoration API nor output validity.
    /// Diagnostics only; callers must not depend on phase stability.
    #[doc(hidden)]
    pub fn inspect(&self) -> Option<Inspection<'_, S>> {
        if self.failure.is_some() {
            return None;
        }
        Some(Inspection {
            scan: &self.scan,
            start: &self.start,
            end: &self.end,
            resume: &self.resume,
            mode: match self.phase {
                Phase::Scan => Mode::Scan,
                Phase::Insert { .. } => Mode::Insert,
                Phase::Compute => Mode::Compute,
                Phase::Emit => Mode::Emit,
                _ => Mode::Other,
            },
            overflow: self.overflow,
        })
    }
    /// Release the exclusive scratch only once the engine is Done and healthy.
    /// Done may coincide with the final Scalar; the caller still owns its
    /// output charge/copy and fresh final admission before publication.
    pub fn into_scratch(self) -> Result<&'w mut Scratch, Error<S::Error>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !matches!(self.phase, Phase::Done) {
            return Err(Error::InvalidState);
        }
        Ok(self.scratch)
    }
    /// Bind caller-supplied live admission, including after cached completion.
    /// A refusal retires this owner permanently; the callback is then inert.
    pub fn check(
        &mut self,
        admit: impl FnOnce() -> Result<(), S::Error>,
    ) -> Result<(), Error<S::Error>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = admit().map_err(Error::Source);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    /// Advance at most the source quantum of 1..=32 admitted transitions.
    /// Emit at most one provisional scalar and latch the original failure.
    pub fn poll<A: Admission<Error = S::Error>>(
        &mut self,
        admission: &mut A,
    ) -> Result<Status, Error<S::Error>>
    where
        S: Reader<A>,
    {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.phase, Phase::Done) {
            return Ok(Status::Complete);
        }
        let result = self.advance(admission);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn advance<A: Admission<Error = S::Error>>(
        &mut self,
        admission: &mut A,
    ) -> Result<Status, Error<S::Error>>
    where
        S: Reader<A>,
    {
        let turns = self.scan.turns();
        if !(1..=32).contains(&turns) {
            return Err(Error::InvalidState);
        }
        for _ in 0..turns {
            admission.step().map_err(Error::Source)?;
            if let Some(status) = self.step(admission)? {
                return Ok(status);
            }
        }
        Ok(Status::Yield)
    }
    fn step<A: Admission<Error = S::Error>>(
        &mut self,
        admission: &mut A,
    ) -> Result<Option<Status>, Error<S::Error>>
    where
        S: Reader<A>,
    {
        match self.phase {
            Phase::Scan => {
                let before = self.scan;
                let read = self.scan.read(admission).map_err(Error::Source)?;
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
                        // Exclude the source End turn from the replay interval.
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
            Phase::Compute => match self.ordered(admission)? {
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
            Phase::Emit => match self.ordered(admission)? {
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
                        if let Some(composed) = S::compose(old, next).map_err(Error::Source)? {
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
    fn mark(&mut self, cell: Cell) -> Result<(), Error<S::Error>> {
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
    fn prepare(&mut self) -> Result<(), Error<S::Error>> {
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
    fn reset_order(&mut self) -> Result<(), Error<S::Error>> {
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
    fn ordered<A: Admission<Error = S::Error>>(
        &mut self,
        admission: &mut A,
    ) -> Result<Ordered, Error<S::Error>>
    where
        S: Reader<A>,
    {
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
        let read = self.scan.read(admission).map_err(Error::Source)?;
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
    fn absorb(&mut self, cell: Cell) -> Result<bool, Error<S::Error>> {
        if let Some(starter) = self.composed {
            if self.last_class == 0 || self.last_class < cell.class {
                if let Some(value) = S::compose(starter, cell.value).map_err(Error::Source)? {
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
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    #[derive(Clone, Copy)]
    struct Input<'a> {
        cells: &'a [Cell],
        index: usize,
        quantum: u8,
    }
    struct Work {
        steps: usize,
        reads: usize,
        left: usize,
    }
    impl Admission for Work {
        type Error = u8;
        fn step(&mut self) -> Result<(), u8> {
            self.left = self.left.checked_sub(1).ok_or(7)?;
            self.steps += 1;
            Ok(())
        }
    }
    impl Source for Input<'_> {
        type Error = u8;
        fn at(&self, other: &Self) -> bool {
            std::ptr::eq(self.cells, other.cells) && self.index == other.index
        }
        fn compose(left: char, right: char) -> Result<Option<char>, u8> {
            Ok(if left == 'e' && right == '\u{301}' {
                Some('é')
            } else {
                None
            })
        }
        fn is_encoding_problem(&self) -> bool {
            false
        }
        fn turns(&self) -> u8 {
            self.quantum
        }
    }
    impl Reader<Work> for Input<'_> {
        fn read(&mut self, work: &mut Work) -> Result<Read, u8> {
            work.left = work.left.checked_sub(1).ok_or(9)?;
            work.reads += 1;
            let cell = self.cells.get(self.index).copied();
            self.index = self.index.checked_add(1).ok_or(9)?;
            Ok(cell.map_or(Read::End, Read::Cell))
        }
    }
    fn drain(cursor: &mut Cursor<'_, Input<'_>>, work: &mut Work) -> String {
        let mut output = String::new();
        for _ in 0..1_000_000 {
            let steps = work.steps;
            let reads = work.reads;
            match cursor.poll(work).unwrap() {
                Status::Scalar(value) => output.push(value),
                Status::Yield => {}
                Status::Complete => return output,
            }
            assert!(work.steps - steps <= 32);
            assert!(work.reads - reads <= 32);
        }
        panic!("bounded fixture did not complete");
    }
    #[test]
    fn stable_order_blocking_overflow_and_quantums_share_one_engine() {
        let mut cells = vec![Cell {
            value: 'e',
            class: 0,
        }];
        cells.extend([
            Cell {
                value: '\u{301}',
                class: 230,
            },
            Cell {
                value: '\u{327}',
                class: 202,
            },
            Cell {
                value: '\u{301}',
                class: 230,
            },
        ]);
        for count in [0, 253, 254, 256, 257, 1024] {
            let mut input = cells.clone();
            input.extend(std::iter::repeat_n(
                Cell {
                    value: '\u{328}',
                    class: 202,
                },
                count,
            ));
            input.push(Cell {
                value: 'x',
                class: 0,
            });
            let expected = format!("é\u{327}{}\u{301}x", "\u{328}".repeat(count));
            for quantum in [1, 32] {
                let mut scratch = Scratch::new();
                let mut cursor = Cursor::new(
                    Input {
                        cells: &input,
                        index: 0,
                        quantum,
                    },
                    &mut scratch,
                );
                let mut work = Work {
                    steps: 0,
                    reads: 0,
                    left: 10_000_000,
                };
                assert_eq!(drain(&mut cursor, &mut work), expected);
                let before = (work.steps, work.reads, work.left);
                assert_eq!(cursor.poll(&mut work), Ok(Status::Complete));
                assert_eq!(before, (work.steps, work.reads, work.left));
                assert!(cursor.into_scratch().is_ok());
            }
        }
        assert_eq!(std::mem::size_of::<Scratch>(), 3072);
    }
    #[derive(Clone, Copy)]
    struct Pausing<'a> {
        input: Input<'a>,
        pause: bool,
    }
    impl Source for Pausing<'_> {
        type Error = u8;
        fn at(&self, other: &Self) -> bool {
            self.input.at(&other.input) && self.pause == other.pause
        }
        fn compose(left: char, right: char) -> Result<Option<char>, u8> {
            if left == 'x' && right == 'y' {
                Ok(Some('z'))
            } else {
                Input::compose(left, right)
            }
        }
        fn is_encoding_problem(&self) -> bool {
            false
        }
        fn turns(&self) -> u8 {
            self.input.quantum
        }
    }
    impl Reader<Work> for Pausing<'_> {
        fn read(&mut self, work: &mut Work) -> Result<Read, u8> {
            if self.pause {
                work.left = work.left.checked_sub(1).ok_or(9)?;
                work.reads += 1;
                self.pause = false;
                Ok(Read::Yield)
            } else {
                let event = self.input.read(work)?;
                self.pause = true;
                Ok(event)
            }
        }
    }
    #[test]
    fn equal_class_blocks_composition_and_yields_restore_exact_replay() {
        for count in [0, 254, 255, 1024] {
            let mut cells = vec![
                Cell {
                    value: 'e',
                    class: 0,
                },
                Cell {
                    value: '\u{300}',
                    class: 230,
                },
                Cell {
                    value: '\u{301}',
                    class: 230,
                },
            ];
            cells.extend(std::iter::repeat_n(
                Cell {
                    value: '\u{328}',
                    class: 202,
                },
                count,
            ));
            cells.extend([
                Cell {
                    value: 'x',
                    class: 0,
                },
                Cell {
                    value: 'y',
                    class: 0,
                },
            ]);
            let expected = format!("e{}\u{300}\u{301}z", "\u{328}".repeat(count));
            for quantum in [1, 32] {
                let mut scratch = Scratch::new();
                let mut cursor = Cursor::new(
                    Pausing {
                        input: Input {
                            cells: &cells,
                            index: 0,
                            quantum,
                        },
                        pause: true,
                    },
                    &mut scratch,
                );
                let mut work = Work {
                    steps: 0,
                    reads: 0,
                    left: 10_000_000,
                };
                let mut output = String::new();
                let mut overflow = false;
                let mut scan_yield = false;
                let mut replay_yield = false;
                let mut complete = false;
                for _ in 0..1_000_000 {
                    let state = cursor.inspect().unwrap();
                    overflow |= state.overflow;
                    let before = (work.steps, work.reads);
                    let paused = state.scan.pause;
                    let was_overflow = state.overflow;
                    let mode = state.mode;
                    match cursor.poll(&mut work).unwrap() {
                        Status::Scalar(value) => output.push(value),
                        Status::Yield => {
                            scan_yield |= paused && mode == Mode::Scan;
                            replay_yield |= paused
                                && was_overflow
                                && matches!(mode, Mode::Compute | Mode::Emit);
                        }
                        Status::Complete => {
                            complete = true;
                            break;
                        }
                    }
                    assert!(work.steps - before.0 <= usize::from(quantum));
                    assert!(work.reads - before.1 <= usize::from(quantum));
                }
                assert!(complete);
                assert_eq!(output, expected);
                assert_eq!(overflow, count + 2 > 256);
                assert!(scan_yield);
                if quantum == 1 && overflow {
                    assert!(replay_yield);
                }
                assert!(cursor.into_scratch().is_ok());
            }
        }
    }
    #[derive(Clone, Copy)]
    struct Inconsistent<'a> {
        input: Input<'a>,
        mode: u8,
        extra: bool,
    }
    impl Source for Inconsistent<'_> {
        type Error = u8;
        fn at(&self, other: &Self) -> bool {
            self.mode != 2 && self.input.at(&other.input) && self.extra == other.extra
        }
        fn compose(left: char, right: char) -> Result<Option<char>, u8> {
            Input::compose(left, right)
        }
        fn is_encoding_problem(&self) -> bool {
            false
        }
        fn turns(&self) -> u8 {
            1
        }
    }
    impl Reader<Work> for Inconsistent<'_> {
        fn read(&mut self, work: &mut Work) -> Result<Read, u8> {
            // Deliberately violate determinism only after the initial scan.
            let replay = work.reads > self.input.cells.len();
            let before = self.input.index;
            let event = self.input.read(work)?;
            if replay && before == 1 {
                if self.mode == 0 {
                    return Ok(Read::Yield);
                }
                if self.mode == 1 && !self.extra {
                    self.input.index = before;
                    self.extra = true;
                }
            }
            Ok(event)
        }
    }
    #[test]
    fn missing_extra_and_unterminated_replay_refuse_without_output() {
        let mut cells = vec![Cell {
            value: 'e',
            class: 0,
        }];
        cells.extend(std::iter::repeat_n(
            Cell {
                value: '\u{327}',
                class: 202,
            },
            257,
        ));
        cells.push(Cell {
            value: '\u{301}',
            class: 230,
        });
        for mode in 0..3 {
            let mut scratch = Scratch::new();
            let mut cursor = Cursor::new(
                Inconsistent {
                    input: Input {
                        cells: &cells,
                        index: 0,
                        quantum: 1,
                    },
                    mode,
                    extra: false,
                },
                &mut scratch,
            );
            let mut work = Work {
                steps: 0,
                reads: 0,
                left: 100_000,
            };
            let mut refused = false;
            for _ in 0..10000 {
                match cursor.poll(&mut work) {
                    Ok(Status::Yield) => {}
                    Ok(_) => panic!("inconsistent replay emitted output"),
                    Err(error) => {
                        assert_eq!(error, Error::InvalidState);
                        assert_eq!(work.reads, if mode < 2 { 518 } else { 519 });
                        let mut fresh = Work {
                            steps: 0,
                            reads: 0,
                            left: 100_000,
                        };
                        assert_eq!(cursor.poll(&mut fresh), Err(error));
                        assert_eq!(
                            cursor.check(|| panic!("admission after refusal")),
                            Err(error)
                        );
                        assert_eq!((fresh.steps, fresh.reads, fresh.left), (0, 0, 100_000));
                        assert!(cursor.inspect().is_none());
                        assert!(matches!(cursor.into_scratch(), Err(Error::InvalidState)));
                        refused = true;
                        break;
                    }
                }
            }
            assert!(refused);
        }
    }
    #[derive(Clone, Copy)]
    struct BrokenComposition<'a>(Input<'a>);
    impl Source for BrokenComposition<'_> {
        type Error = u8;
        fn at(&self, other: &Self) -> bool {
            self.0.at(&other.0)
        }
        fn compose(_: char, _: char) -> Result<Option<char>, u8> {
            Err(13)
        }
        fn is_encoding_problem(&self) -> bool {
            false
        }
        fn turns(&self) -> u8 {
            1
        }
    }
    impl Reader<Work> for BrokenComposition<'_> {
        fn read(&mut self, work: &mut Work) -> Result<Read, u8> {
            self.0.read(work)
        }
    }
    #[test]
    fn empty_leading_marks_and_composition_refusal_preserve_ownership() {
        for (cells, expected) in [
            (vec![], String::new()),
            (
                vec![
                    Cell {
                        value: '\u{301}',
                        class: 230,
                    },
                    Cell {
                        value: '\u{327}',
                        class: 202,
                    },
                    Cell {
                        value: '\u{301}',
                        class: 230,
                    },
                ],
                "\u{327}\u{301}\u{301}".to_owned(),
            ),
        ] {
            let mut scratch = Scratch::new();
            let mut cursor = Cursor::new(
                Input {
                    cells: &cells,
                    index: 0,
                    quantum: 1,
                },
                &mut scratch,
            );
            let mut work = Work {
                steps: 0,
                reads: 0,
                left: 1000,
            };
            assert_eq!(drain(&mut cursor, &mut work), expected);
            assert!(cursor.check(|| Ok(())).is_ok());
            assert!(cursor.into_scratch().is_ok());
        }
        let cells = [
            Cell {
                value: 'e',
                class: 0,
            },
            Cell {
                value: '\u{301}',
                class: 230,
            },
        ];
        let mut scratch = Scratch::new();
        let mut cursor = Cursor::new(
            BrokenComposition(Input {
                cells: &cells,
                index: 0,
                quantum: 1,
            }),
            &mut scratch,
        );
        let mut work = Work {
            steps: 0,
            reads: 0,
            left: 1000,
        };
        for _ in 0..100 {
            match cursor.poll(&mut work) {
                Ok(Status::Yield) => {}
                Ok(_) => panic!("output after composition refusal"),
                Err(error) => {
                    assert_eq!(error, Error::Source(13));
                    let before = (work.steps, work.reads, work.left);
                    assert_eq!(cursor.poll(&mut work), Err(error));
                    assert_eq!(
                        cursor.check(|| panic!("fresh admission after refusal")),
                        Err(error)
                    );
                    assert!(cursor.inspect().is_none());
                    assert_eq!(before, (work.steps, work.reads, work.left));
                    assert!(matches!(cursor.into_scratch(), Err(Error::Source(13))));
                    return;
                }
            }
        }
        panic!("composition refusal was not reached");
    }
    #[test]
    fn admission_refusals_are_sticky_and_cached_completion_needs_live_check() {
        let cells = [
            Cell {
                value: 'e',
                class: 0,
            },
            Cell {
                value: '\u{301}',
                class: 230,
            },
            Cell {
                value: 'x',
                class: 0,
            },
        ];
        let mut scratch = Scratch::new();
        let mut cursor = Cursor::new(
            Input {
                cells: &cells,
                index: 0,
                quantum: 1,
            },
            &mut scratch,
        );
        let mut work = Work {
            steps: 0,
            reads: 0,
            left: 10_000,
        };
        assert_eq!(drain(&mut cursor, &mut work), "éx");
        assert_eq!((work.steps, work.reads), (9, 4));
        let total = work.steps + work.reads;
        assert_eq!(cursor.check(|| Err(11)), Err(Error::Source(11)));
        assert!(cursor.inspect().is_none());
        assert_eq!(cursor.poll(&mut work), Err(Error::Source(11)));
        assert!(matches!(cursor.into_scratch(), Err(Error::Source(11))));
        for cut in 0..total {
            let mut scratch = Scratch::new();
            let mut cursor = Cursor::new(
                Input {
                    cells: &cells,
                    index: 0,
                    quantum: 1,
                },
                &mut scratch,
            );
            let mut work = Work {
                steps: 0,
                reads: 0,
                left: cut,
            };
            let error = loop {
                match cursor.poll(&mut work) {
                    Err(error) => break error,
                    Ok(Status::Complete) => panic!("premature complete"),
                    Ok(_) => {}
                }
            };
            assert!(matches!(error, Error::Source(7 | 9)));
            let mut replacement = Work {
                steps: 0,
                reads: 0,
                left: 10_000,
            };
            assert_eq!(cursor.poll(&mut replacement), Err(error));
            assert_eq!(
                cursor.check(|| panic!("admission after failure")),
                Err(error)
            );
            assert_eq!((replacement.steps, replacement.reads), (0, 0));
        }
        for quantum in [0, 33] {
            let mut scratch = Scratch::new();
            let mut cursor = Cursor::new(
                Input {
                    cells: &cells,
                    index: 0,
                    quantum,
                },
                &mut scratch,
            );
            assert_eq!(cursor.poll(&mut work), Err(Error::InvalidState));
        }
    }
}
