//! Resident raw-line delimiter scanning; entity/source authority is external.
use crate::{
    admission::work::{Charge, Meter, Stop},
    ports::Tick,
};
use td_header::{
    mime_boundary::{State, MAX_BOUNDARY_BYTES},
    mime_protocol::{Kind, Validator},
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Delimiter {
    pub line_start: u64,
    pub after_line: u64,
    pub preceding_end: u64,
    pub closing: bool,
    pub ignored_suffix: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Delimiter(Delimiter),
    Complete,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Work(Stop),
    InvalidBoundary,
    InvalidRange,
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Work(_) => "MIME delimiter work refusal",
            Self::InvalidBoundary => "invalid MIME boundary",
            Self::InvalidRange => "invalid MIME delimiter source range",
            Self::InvalidState => "invalid MIME delimiter state",
        })
    }
}
impl std::error::Error for Error {}
const BODY_BYTES_PER_TURN: usize = 128;

/// Events are provisional offsets, never part/tree/blob publication authority.
/// Every input is a complete authorized entity body, not a captured prefix.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_delimiter::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_delimiter::Cursor<'_, '_>>();
/// ```
pub struct Cursor<'a, 'w> {
    core: Core<'a>,
    boundary: &'a [u8],
    work: &'w mut Meter,
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub fn new(
        source: &'a [u8],
        base: u64,
        boundary: &'a [u8],
        work: &'w mut Meter,
    ) -> Result<Self, Error> {
        Ok(Self {
            core: Core::new(source, base, boundary.len())?,
            boundary,
            work,
        })
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.core.check_deadline(now, self.work)
    }
    pub fn finish(mut self, now: Tick) -> Result<&'w mut Meter, Error> {
        self.check_deadline(now)?;
        if !self.core.complete {
            return Err(Error::InvalidState);
        }
        Ok(self.work)
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        self.core.poll(now, self.work, self.boundary)
    }
}
/// Private progress; its owner pins the immutable pattern and original meter.
/// No live admission reference is retained across calls.
pub(crate) struct Core<'a> {
    source: &'a [u8],
    line: State,
    base: u64,
    position: usize,
    line_start: usize,
    preceding_ending: usize,
    pending_cr: bool,
    validation: usize,
    validator: Validator,
    validated: bool,
    complete: bool,
    failure: Option<Error>,
}
impl<'a> Core<'a> {
    pub(crate) fn new(source: &'a [u8], base: u64, boundary_len: usize) -> Result<Self, Error> {
        base.checked_add(u64::try_from(source.len()).map_err(|_| Error::InvalidRange)?)
            .ok_or(Error::InvalidRange)?;
        Ok(Self {
            source,
            line: State::new(boundary_len).ok_or(Error::InvalidBoundary)?,
            base,
            position: 0,
            line_start: 0,
            preceding_ending: 0,
            pending_cr: false,
            validation: 0,
            validator: Validator::new(Kind::Boundary),
            validated: false,
            complete: false,
            failure: None,
        })
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
            self.complete = false;
        }
        result
    }
    pub(crate) fn check_deadline(&mut self, now: Tick, work: &mut Meter) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = work.charge(now, Charge::default()).map_err(Error::Work);
        self.outcome(result)
    }
    pub(crate) fn poll(
        &mut self,
        now: Tick,
        work: &mut Meter,
        boundary: &[u8],
    ) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.complete {
            return Ok(Status::Complete);
        }
        let result = if boundary.len() != self.line.boundary_len() {
            Err(Error::InvalidState)
        } else {
            self.step(now, work, boundary)
        };
        self.outcome(result)
    }
    fn offset(&self, position: usize) -> Result<u64, Error> {
        self.base
            .checked_add(u64::try_from(position).map_err(|_| Error::InvalidRange)?)
            .ok_or(Error::InvalidRange)
    }
    fn end_line(&mut self, ending: usize) -> Result<Option<Delimiter>, Error> {
        let result = if let Some(found) = self.line.finish() {
            Some(Delimiter {
                line_start: self.offset(self.line_start)?,
                after_line: self.offset(self.position)?,
                // The child owner clamps this delimiter-leading ending at its start.
                preceding_end: self.offset(
                    self.line_start
                        .checked_sub(self.preceding_ending)
                        .ok_or(Error::InvalidState)?,
                )?,
                closing: found.closing,
                ignored_suffix: found.ignored_suffix,
            })
        } else {
            None
        };
        self.line_start = self.position;
        self.preceding_ending = ending;
        self.pending_cr = false;
        self.line.reset();
        Ok(result)
    }
    fn step(&mut self, now: Tick, work: &mut Meter, boundary: &[u8]) -> Result<Status, Error> {
        work.charge(
            now,
            Charge {
                records: 1,
                ..Charge::default()
            },
        )
        .map_err(Error::Work)?;
        if !self.validated {
            for _ in 0..MAX_BOUNDARY_BYTES {
                if self.validation == self.line.boundary_len() {
                    break;
                }
                work.charge(
                    now,
                    Charge {
                        io_bytes: 1,
                        ..Charge::default()
                    },
                )
                .map_err(Error::Work)?;
                let byte = *boundary.get(self.validation).ok_or(Error::InvalidState)?;
                self.validator.feed(byte);
                self.validation += 1;
            }
            if self.validation != self.line.boundary_len() {
                return Err(Error::InvalidState);
            }
            if !self.validator.is_valid() {
                return Err(Error::InvalidBoundary);
            }
            self.validated = true;
            return Ok(Status::Yield);
        }
        for _ in 0..BODY_BYTES_PER_TURN {
            if self.position == self.source.len() {
                if self.line_start == self.position && !self.pending_cr {
                    self.complete = true;
                    return Ok(Status::Complete);
                }
                if self.pending_cr {
                    self.line.feed(b'\r', boundary);
                    self.pending_cr = false;
                }
                if let Some(found) = self.end_line(0)? {
                    return Ok(Status::Delimiter(found));
                }
                self.complete = true;
                return Ok(Status::Complete);
            }
            // One source read plus at most one trusted boundary comparison.
            work.charge(
                now,
                Charge {
                    io_bytes: 2,
                    ..Charge::default()
                },
            )
            .map_err(Error::Work)?;
            let byte = *self.source.get(self.position).ok_or(Error::InvalidState)?;
            self.position += 1;
            if byte == b'\n' {
                let ending = if self.pending_cr { 2 } else { 1 };
                if let Some(found) = self.end_line(ending)? {
                    return Ok(Status::Delimiter(found));
                }
            } else {
                if self.pending_cr {
                    self.line.feed(b'\r', boundary);
                    self.pending_cr = false;
                }
                if byte == b'\r' {
                    self.pending_cr = true;
                } else {
                    self.line.feed(byte, boundary);
                }
            }
        }
        Ok(Status::Yield)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::ports::Deadline;
    fn meter() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 1_000_000,
                records: 1_000_000,
                ..Charge::default()
            },
        )
    }
    fn drain(cursor: &mut Cursor<'_, '_>) -> Result<Vec<Delimiter>, Error> {
        let mut found = Vec::new();
        for _ in 0..100_000 {
            match cursor.poll(Tick(1))? {
                Status::Yield => {}
                Status::Delimiter(d) => found.push(d),
                Status::Complete => return Ok(found),
            }
        }
        panic!("delimiter scan did not finish")
    }
    #[test]
    fn moved_private_progress_matches_public_charges_and_events() {
        let source = b"pre\r\n--a\r\nbody\n--a--tail";
        let mut public_work = meter();
        let mut private_work = meter();
        let mut public = Cursor::new(source, 7, b"a", &mut public_work).unwrap();
        let mut private = Core::new(source, 7, 1).unwrap();
        for _ in 0..1000 {
            let expected = public.poll(Tick(1)).unwrap();
            private = std::hint::black_box(private);
            assert_eq!(
                private.poll(Tick(1), &mut private_work, b"a").unwrap(),
                expected
            );
            assert_eq!(private_work.remaining(), public.work.remaining());
            if expected == Status::Complete {
                assert_eq!(
                    private.check_deadline(Tick(100), &mut private_work),
                    Err(Error::Work(Stop::Deadline))
                );
                let before = private_work.remaining();
                assert_eq!(
                    private.poll(Tick(1), &mut private_work, b"a"),
                    Err(Error::Work(Stop::Deadline))
                );
                assert_eq!(private_work.remaining(), before);
                return;
            }
        }
        panic!("core did not finish");
    }
    #[test]
    fn private_boundary_length_misuse_refuses_without_charging() {
        let source = [b'x'; 300];
        for active_turns in 0..=2 {
            let mut work = meter();
            let mut core = Core::new(&source, 0, 1).unwrap();
            for _ in 0..active_turns {
                assert_eq!(core.poll(Tick(1), &mut work, b"a"), Ok(Status::Yield));
            }
            let before = work.remaining();
            assert_eq!(
                core.poll(Tick(1), &mut work, b"ab"),
                Err(Error::InvalidState)
            );
            assert_eq!(work.remaining(), before);
            assert_eq!(
                core.poll(Tick(1), &mut work, b"a"),
                Err(Error::InvalidState)
            );
            assert_eq!(
                core.check_deadline(Tick(1), &mut work),
                Err(Error::InvalidState)
            );
            assert_eq!(work.remaining(), before);
        }
    }
    // Independent whole-line oracle, intentionally allocating only in tests.
    fn reference(source: &[u8], base: u64, boundary: &[u8]) -> Vec<Delimiter> {
        let mut found = Vec::new();
        let mut start = 0;
        let mut preceding = 0;
        let prefix = [b"--".as_slice(), boundary].concat();
        for raw in source.split_inclusive(|b| *b == b'\n') {
            let mut ending = 0;
            let mut line = raw;
            if let Some(head) = line.strip_suffix(b"\n") {
                line = head;
                ending = 1;
                if let Some(head) = line.strip_suffix(b"\r") {
                    line = head;
                    ending = 2;
                }
            }
            if let Some(tail) = line.strip_prefix(prefix.as_slice()) {
                let (closing, suffix) = if let Some(suffix) = tail.strip_prefix(b"--") {
                    (true, suffix)
                } else {
                    (false, tail)
                };
                found.push(Delimiter {
                    line_start: base + start as u64,
                    after_line: base + (start + raw.len()) as u64,
                    preceding_end: base + (start - preceding) as u64,
                    closing,
                    ignored_suffix: suffix.iter().any(|b| !matches!(b, b' ' | b'\t')),
                });
            }
            start += raw.len();
            preceding = ending;
        }
        found
    }
    #[test]
    fn line_extents_and_suffix_recovery_match_independent_oracle() {
        const {
            assert!(std::mem::size_of::<Cursor<'_, '_>>() <= 160);
        }
        for source in [
            b"".as_slice(),
            b"preamble\r\n--b\r\nA: B\r\n\r\nbody\r\n--b--\r\nepilogue",
            b"--b\n--b\n--b--",
            b"--b\r",
            b"bare\r--b\n--b---tail\n",
            b" --b\n--boops\r\n--b \t\n--b-- \t\n",
            b"--bx\n--b-y\n--b--z\n",
        ] {
            let mut work = meter();
            let initial = work.remaining();
            let identities = std::ptr::from_ref(&work);
            let mut cursor = Cursor::new(source, 17, b"b", &mut work).unwrap();

            assert_eq!(drain(&mut cursor).unwrap(), reference(source, 17, b"b"));
            assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
            let work = cursor.finish(Tick(1)).unwrap();
            assert_eq!(std::ptr::from_ref(work), identities);
            assert_eq!(
                initial.io_bytes - work.remaining().io_bytes,
                1 + 2 * source.len() as u64
            );
        }
        // A maximal boundary and CR at the 128-byte turn seam are preserved.
        let boundary = [b'x'; 70];
        for prefix in 0..=260 {
            for ending in [b"\r\n".as_slice(), b"\n", b"\r"] {
                let mut source = vec![b'a'; prefix];
                source.extend_from_slice(ending);
                source.extend_from_slice(b"--");
                source.extend_from_slice(&boundary);
                source.extend_from_slice(b"-- \t");
                source.extend_from_slice(ending);
                let mut work = meter();
                let mut cursor = Cursor::new(&source, 0, &boundary, &mut work).unwrap();
                let before = cursor.work.remaining();
                assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
                assert!(cursor.core.validated);
                assert_eq!(before.records - cursor.work.remaining().records, 1);
                assert_eq!(before.io_bytes - cursor.work.remaining().io_bytes, 70);
                assert_eq!(
                    drain(&mut cursor).unwrap(),
                    reference(&source, 0, &boundary)
                );
            }
            let mut source = vec![b'a'; prefix];
            source.extend_from_slice(b"\r");
            let mut work = meter();
            let mut cursor = Cursor::new(&source, 0, b"b", &mut work).unwrap();
            assert_eq!(drain(&mut cursor).unwrap(), reference(&source, 0, b"b"));
        }
        let mut invalid = [b'x'; 70];
        *invalid.get_mut(69).unwrap() = b' ';
        for limit in 0..=70 {
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: limit,
                    records: 100,
                    ..Charge::default()
                },
            );
            let mut cursor = Cursor::new(b"", 0, &invalid, &mut work).unwrap();
            assert_eq!(
                drain(&mut cursor),
                Err(if limit < 70 {
                    Error::Work(Stop::IoBytes)
                } else {
                    Error::InvalidBoundary
                })
            );
        }
    }
    #[test]
    fn original_work_cuts_live_deadlines_and_invalid_inputs_are_sticky() {
        let source = b"pre\r\n--b\r\nbody\n--b--tail";
        let mut work = meter();
        let initial = work.remaining();
        let mut turns = 0;
        {
            let mut cursor = Cursor::new(source, 0, b"b", &mut work).unwrap();
            for _ in 0..1000 {
                let before = cursor.work.remaining();
                let status = cursor.poll(Tick(1)).unwrap();
                turns += 1;
                let after = cursor.work.remaining();
                assert!(before.io_bytes - after.io_bytes <= 256);
                assert!(before.records - after.records <= 1);
                if matches!(status, Status::Complete) {
                    break;
                }
            }
            assert!(cursor.core.complete);
        }
        for (total, stop) in [
            (initial.io_bytes - work.remaining().io_bytes, Stop::IoBytes),
            (initial.records - work.remaining().records, Stop::Records),
        ] {
            assert!(total > 0);
            for limit in 0..total {
                let mut charge = initial;
                match stop {
                    Stop::IoBytes => charge.io_bytes = limit,
                    Stop::Records => charge.records = limit,
                    _ => panic!("unexpected cut"),
                }
                let mut work = Meter::new(Deadline::after(Tick(0), 100).unwrap(), charge);
                let mut cursor = Cursor::new(source, 0, b"b", &mut work).unwrap();
                assert_eq!(drain(&mut cursor), Err(Error::Work(stop)));
                let remaining = cursor.work.remaining();
                assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(stop)));
                assert_eq!(cursor.check_deadline(Tick(1)), Err(Error::Work(stop)));
                assert_eq!(cursor.work.remaining(), remaining);
                assert!(cursor.finish(Tick(1)).is_err());
            }
        }
        for turn in 0..=turns {
            for via_poll in [false, true] {
                let mut work = meter();
                let mut cursor = Cursor::new(source, 0, b"b", &mut work).unwrap();
                for _ in 0..turn {
                    cursor.poll(Tick(1)).unwrap();
                }
                let error = Error::Work(Stop::Deadline);
                if via_poll && turn < turns {
                    assert_eq!(cursor.poll(Tick(100)), Err(error));
                } else {
                    assert_eq!(cursor.check_deadline(Tick(100)), Err(error));
                }
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert!(cursor.finish(Tick(1)).is_err());
            }
        }
        for boundary in [b"bad ".as_slice(), b"a\r", b"a\xff"] {
            let mut work = meter();
            let mut cursor = Cursor::new(source, 0, boundary, &mut work).unwrap();
            assert_eq!(drain(&mut cursor), Err(Error::InvalidBoundary));
            assert_eq!(cursor.poll(Tick(1)), Err(Error::InvalidBoundary));
        }
        let mut work = meter();
        assert!(matches!(
            Cursor::new(source, u64::MAX, b"b", &mut work),
            Err(Error::InvalidRange)
        ));
        assert!(matches!(
            Cursor::new(source, 0, b"", &mut work),
            Err(Error::InvalidBoundary)
        ));
        let cursor = Cursor::new(source, 0, b"b", &mut work).unwrap();
        assert!(matches!(cursor.finish(Tick(1)), Err(Error::InvalidState)));
    }
}
