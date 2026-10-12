//! Incremental fixed-record external sorting with caller-owned custody and work.
use std::cmp::Ordering;

pub const MAX_RECORD_BYTES: usize = 2048;
pub const MAX_RUN_RECORDS: usize = 256;
pub const MAX_MERGE_RUNS: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum File {
    A,
    B,
}
impl File {
    fn other(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }
}
/// A source may yield after a filtered-out candidate without ending the scan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Next {
    Record,
    Skipped,
    Complete,
}
/// Trusted adapter: both files belong exclusively to this job. All methods use
/// its original cumulative work meter, deadline and retained source view.
/// `admit` charges one turn plus the stated I/O bytes before any operation.
/// File methods must retain their quota and fail closed; reads fill exactly.
/// `next` and `compare` must themselves have bounded work. The comparator must
/// give a consistent total order, including the query's deterministic ID tie.
pub trait Job<const N: usize> {
    type Error;
    fn admit(&mut self, io_bytes: u64) -> Result<(), Self::Error>;
    fn next(&mut self, record: &mut [u8; N]) -> Result<Next, Self::Error>;
    fn compare(&mut self, a: &[u8; N], b: &[u8; N]) -> Result<Ordering, Self::Error>;
    fn append(&mut self, file: File, record: &[u8; N]) -> Result<(), Self::Error>;
    fn read(&mut self, file: File, offset: u64, record: &mut [u8; N]) -> Result<(), Self::Error>;
    fn reset(&mut self, file: File) -> Result<(), Self::Error>;
    fn seal(&mut self, file: File) -> Result<u64, Self::Error>;
}
#[derive(Debug, Eq, PartialEq)]
pub enum Error<E> {
    Invalid,
    Overflow,
    Length,
    Failed,
    Completed,
    Backend(E),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Output {
    pub file: File,
    pub records: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Progress {
    Pending,
    Complete(Output),
}
#[derive(Clone, Copy, Debug)]
enum HeapPhase {
    Build(usize),
    Extract(usize),
}
#[derive(Clone, Copy, Debug)]
enum State {
    ResetA,
    Fill,
    Heap(HeapPhase),
    Sift {
        root: usize,
        end: usize,
        after: HeapPhase,
    },
    SiftChosen {
        root: usize,
        end: usize,
        child: usize,
        after: HeapPhase,
    },
    Flush(usize),
    SealInitial,
    ResetPass,
    Group,
    Load(usize),
    Select {
        slot: usize,
        best: Option<usize>,
    },
    Emit(usize),
    Reload(usize),
    SealPass,
    Complete,
    Completed,
    Failed,
}
/// Each poll performs at most one comparison or one bounded backend operation.
/// An error retires this sorter; discard both scratch files, except Completed
/// after a successful completion, which leaves the returned output valid.
/// Completion is only sorted scratch, never permission to publish a response.
pub struct Sorter<'a, const N: usize> {
    run: &'a mut [[u8; N]],
    heads: &'a mut [[u8; N]],
    state: State,
    length: usize,
    count: u64,
    eof: bool,
    input: File,
    width: u64,
    group: u64,
    cursor: [u64; MAX_MERGE_RUNS],
    end: [u64; MAX_MERGE_RUNS],
}
impl<const N: usize> std::fmt::Debug for Sorter<'_, N> {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("Sorter")
            .field("state", &self.state)
            .field("count", &self.count)
            .field("width", &self.width)
            .field("input", &self.input)
            .finish_non_exhaustive()
    }
}
impl<'a, const N: usize> Sorter<'a, N> {
    pub fn new(
        run: &'a mut [[u8; N]],
        heads: &'a mut [[u8; N]],
    ) -> Result<Self, Error<std::convert::Infallible>> {
        if N == 0
            || N > MAX_RECORD_BYTES
            || run.is_empty()
            || run.len() > MAX_RUN_RECORDS
            || heads.len() != MAX_MERGE_RUNS
        {
            return Err(Error::Invalid);
        }
        Ok(Self {
            width: run.len() as u64,
            run,
            heads,
            state: State::ResetA,
            length: 0,
            count: 0,
            eof: false,
            input: File::A,
            group: 0,
            cursor: [0; MAX_MERGE_RUNS],
            end: [0; MAX_MERGE_RUNS],
        })
    }
    fn bytes<E>(&self) -> Result<u64, Error<E>> {
        self.count.checked_mul(N as u64).ok_or(Error::Overflow)
    }
    fn offset<E>(record: u64) -> Result<u64, Error<E>> {
        record.checked_mul(N as u64).ok_or(Error::Overflow)
    }
    fn swap<E>(&mut self, a: usize, b: usize) -> Result<(), Error<E>> {
        if a == b {
            return Ok(());
        }
        let (low, high) = if a < b { (a, b) } else { (b, a) };
        let (left, right) = self.run.split_at_mut_checked(high).ok_or(Error::Invalid)?;
        std::mem::swap(
            left.get_mut(low).ok_or(Error::Invalid)?,
            right.first_mut().ok_or(Error::Invalid)?,
        );
        Ok(())
    }
    pub fn poll<J: Job<N>>(&mut self, job: &mut J) -> Result<Progress, Error<J::Error>> {
        let state = std::mem::replace(&mut self.state, State::Failed);
        if matches!(state, State::Completed) {
            self.state = State::Completed;
            return Err(Error::Completed);
        }
        if matches!(state, State::Failed) {
            return Err(Error::Failed);
        }
        let io = match state {
            State::Fill | State::Emit(_) => N as u64,
            State::Flush(index) if index < self.length => N as u64,
            State::Load(slot) | State::Reload(slot)
                if self
                    .cursor
                    .get(slot)
                    .zip(self.end.get(slot))
                    .is_some_and(|(cursor, end)| cursor < end) =>
            {
                N as u64
            }
            _ => 0,
        };
        job.admit(io).map_err(Error::Backend)?;
        let next = match state {
            State::ResetA => {
                job.reset(File::A).map_err(Error::Backend)?;
                State::Fill
            }
            State::Fill => {
                let record = self.run.get_mut(self.length).ok_or(Error::Invalid)?;
                match job.next(record).map_err(Error::Backend)? {
                    Next::Record => {
                        self.count = self.count.checked_add(1).ok_or(Error::Overflow)?;
                        self.bytes()?;
                        self.length += 1;
                    }
                    Next::Skipped => {}
                    Next::Complete => self.eof = true,
                }
                if self.eof && self.length == 0 {
                    State::SealInitial
                } else if self.eof || self.length == self.run.len() {
                    State::Heap(HeapPhase::Build(self.length / 2))
                } else {
                    State::Fill
                }
            }
            State::Heap(HeapPhase::Build(root)) => {
                if root == 0 {
                    State::Heap(HeapPhase::Extract(self.length))
                } else {
                    State::Sift {
                        root: root - 1,
                        end: self.length,
                        after: HeapPhase::Build(root - 1),
                    }
                }
            }
            State::Heap(HeapPhase::Extract(end)) => {
                if end <= 1 {
                    State::Flush(0)
                } else {
                    self.swap(0, end - 1)?;
                    State::Sift {
                        root: 0,
                        end: end - 1,
                        after: HeapPhase::Extract(end - 1),
                    }
                }
            }
            State::Sift { root, end, after } => {
                let child = root
                    .checked_mul(2)
                    .and_then(|n| n.checked_add(1))
                    .ok_or(Error::Overflow)?;
                if child >= end {
                    State::Heap(after)
                } else {
                    let right = child.checked_add(1).ok_or(Error::Overflow)?;
                    let child = if right < end
                        && job
                            .compare(
                                self.run.get(child).ok_or(Error::Invalid)?,
                                self.run.get(right).ok_or(Error::Invalid)?,
                            )
                            .map_err(Error::Backend)?
                            == Ordering::Less
                    {
                        right
                    } else {
                        child
                    };
                    State::SiftChosen {
                        root,
                        end,
                        child,
                        after,
                    }
                }
            }
            State::SiftChosen {
                root,
                end,
                child,
                after,
            } => {
                if job
                    .compare(
                        self.run.get(root).ok_or(Error::Invalid)?,
                        self.run.get(child).ok_or(Error::Invalid)?,
                    )
                    .map_err(Error::Backend)?
                    == Ordering::Less
                {
                    self.swap(root, child)?;
                    State::Sift {
                        root: child,
                        end,
                        after,
                    }
                } else {
                    State::Heap(after)
                }
            }
            State::Flush(index) => {
                if index < self.length {
                    job.append(File::A, self.run.get(index).ok_or(Error::Invalid)?)
                        .map_err(Error::Backend)?;
                    State::Flush(index + 1)
                } else {
                    self.length = 0;
                    if self.eof {
                        State::SealInitial
                    } else {
                        State::Fill
                    }
                }
            }
            State::SealInitial => {
                if job.seal(File::A).map_err(Error::Backend)? != self.bytes()? {
                    return Err(Error::Length);
                }
                if self.count <= self.width {
                    State::Complete
                } else {
                    State::ResetPass
                }
            }
            State::ResetPass => {
                job.reset(self.input.other()).map_err(Error::Backend)?;
                self.group = 0;
                State::Group
            }
            State::Group => {
                if self.group >= self.count {
                    State::SealPass
                } else {
                    let mut start = self.group;
                    for (cursor, end) in self.cursor.iter_mut().zip(self.end.iter_mut()) {
                        *cursor = start;
                        start = start.saturating_add(self.width).min(self.count);
                        *end = start;
                    }
                    State::Load(0)
                }
            }
            State::Load(slot) => {
                if slot == MAX_MERGE_RUNS {
                    State::Select {
                        slot: 0,
                        best: None,
                    }
                } else {
                    let cursor = *self.cursor.get(slot).ok_or(Error::Invalid)?;
                    if cursor < *self.end.get(slot).ok_or(Error::Invalid)? {
                        job.read(
                            self.input,
                            Self::offset(cursor)?,
                            self.heads.get_mut(slot).ok_or(Error::Invalid)?,
                        )
                        .map_err(Error::Backend)?;
                    }
                    State::Load(slot + 1)
                }
            }
            State::Select { slot, best } => {
                if slot == MAX_MERGE_RUNS {
                    match best {
                        Some(best) => State::Emit(best),
                        None => {
                            self.group = *self.end.last().ok_or(Error::Invalid)?;
                            State::Group
                        }
                    }
                } else {
                    let active = self.cursor.get(slot).ok_or(Error::Invalid)?
                        < self.end.get(slot).ok_or(Error::Invalid)?;
                    let chosen = if active {
                        match best {
                            None => Some(slot),
                            Some(best) => {
                                let order = job
                                    .compare(
                                        self.heads.get(slot).ok_or(Error::Invalid)?,
                                        self.heads.get(best).ok_or(Error::Invalid)?,
                                    )
                                    .map_err(Error::Backend)?;
                                if order == Ordering::Less {
                                    Some(slot)
                                } else {
                                    Some(best)
                                }
                            }
                        }
                    } else {
                        best
                    };
                    State::Select {
                        slot: slot + 1,
                        best: chosen,
                    }
                }
            }
            State::Emit(slot) => {
                job.append(
                    self.input.other(),
                    self.heads.get(slot).ok_or(Error::Invalid)?,
                )
                .map_err(Error::Backend)?;
                let cursor = self.cursor.get_mut(slot).ok_or(Error::Invalid)?;
                *cursor = cursor.checked_add(1).ok_or(Error::Overflow)?;
                State::Reload(slot)
            }
            State::Reload(slot) => {
                let cursor = *self.cursor.get(slot).ok_or(Error::Invalid)?;
                if cursor < *self.end.get(slot).ok_or(Error::Invalid)? {
                    job.read(
                        self.input,
                        Self::offset(cursor)?,
                        self.heads.get_mut(slot).ok_or(Error::Invalid)?,
                    )
                    .map_err(Error::Backend)?;
                }
                State::Select {
                    slot: 0,
                    best: None,
                }
            }
            State::SealPass => {
                if job.seal(self.input.other()).map_err(Error::Backend)? != self.bytes()? {
                    return Err(Error::Length);
                }
                self.input = self.input.other();
                self.width = self
                    .width
                    .saturating_mul(MAX_MERGE_RUNS as u64)
                    .min(self.count);
                if self.width == self.count {
                    State::Complete
                } else {
                    State::ResetPass
                }
            }
            State::Complete => {
                self.state = State::Completed;
                return Ok(Progress::Complete(Output {
                    file: self.input,
                    records: self.count,
                }));
            }
            State::Completed => return Err(Error::Completed),
            State::Failed => return Err(Error::Failed),
        };
        self.state = next;
        Ok(Progress::Pending)
    }
}
#[cfg(test)]
#[path = "external_sort_tests.rs"]
mod tests;
