//! Bounded raw-line delimiter scanning; entity/source authority is external.
use crate::{
    time::Tick,
    work::{Charge, Meter, Owner, Stop},
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
    InvalidOwner,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Work(_) => "MIME delimiter work refusal",
            Self::InvalidBoundary => "invalid MIME boundary",
            Self::InvalidRange => "invalid MIME delimiter source range",
            Self::InvalidState => "invalid MIME delimiter state",
            Self::InvalidOwner => "MIME delimiter work owner mismatch",
        })
    }
}
impl std::error::Error for Error {}
const BODY_BYTES_PER_TURN: usize = 128;

/// Events are provisional offsets, never part/tree/blob publication authority.
/// Every input is a complete authorized entity body, not a captured prefix.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mime::delimiter::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mime::delimiter::Cursor<'_, '_>>();
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
        if !self.core.state.complete {
            return Err(Error::InvalidState);
        }
        Ok(self.work)
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        self.core.poll(now, self.work, self.boundary)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamStatus {
    Yield,
    NeedInput,
    Delimiter(Delimiter),
    Complete,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Progress {
    /// Input bytes consumed this turn, at most 128; retain the remainder.
    pub consumed: usize,
    pub status: StreamStatus,
}
/// Same grammar over contiguous fragments of one immutable, admitted extent.
/// Length is the exact entity-body extent, not the available captured prefix.
/// Caller supplies every byte in order and retains source identity/custody.
/// No fragment or complete body is retained. Cached Complete is passive;
/// check_deadline and consuming finish freshly admit under the original meter.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mime::delimiter::Stream<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mime::delimiter::Stream<'_, '_>>();
/// ```
pub struct Stream<'a, 'w> {
    state: Scan,
    length: u64,
    boundary: &'a [u8],
    work: &'w mut Meter,
}
impl<'a, 'w> Stream<'a, 'w> {
    pub fn new(
        base: u64,
        length: u64,
        boundary: &'a [u8],
        work: &'w mut Meter,
    ) -> Result<Self, Error> {
        Ok(Self {
            state: Scan::new(base, length, boundary.len())?,
            length,
            boundary,
            work,
        })
    }
    pub fn poll(&mut self, input: &[u8], now: Tick) -> Result<Progress, Error> {
        self.state
            .poll(input, self.length, now, self.work, self.boundary)
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.state.check_deadline(now, self.work)
    }
    pub fn finish(mut self, now: Tick) -> Result<&'w mut Meter, Error> {
        self.check_deadline(now)?;
        if !self.state.complete {
            return Err(Error::InvalidState);
        }
        Ok(self.work)
    }
}

/// Composable scanner with an owned boundary and a move-stable work binding.
/// Caller retains the original meter, immutable source and contiguous fragments.
/// No copied progress, boundary replacement or fresh meter can renew admission.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mime::delimiter::Scanner>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mime::delimiter::Scanner>();
/// ```
pub struct Scanner {
    state: Scan,
    length: u64,
    boundary: [u8; MAX_BOUNDARY_BYTES],
    boundary_len: usize,
    owner: Owner,
}
impl Scanner {
    pub fn new(
        base: u64,
        length: u64,
        boundary: &[u8],
        now: Tick,
        work: &mut Meter,
    ) -> Result<Self, Error> {
        let state = Scan::new(base, length, boundary.len())?;
        work.charge(now, Charge::default()).map_err(Error::Work)?;
        let owner = work.owner().ok_or(Error::InvalidOwner)?;
        work.charge(
            now,
            Charge {
                io_bytes: boundary.len() as u64,
                records: 1,
                ..Charge::default()
            },
        )
        .map_err(Error::Work)?;
        let mut retained = [0; MAX_BOUNDARY_BYTES];
        retained
            .get_mut(..boundary.len())
            .ok_or(Error::InvalidBoundary)?
            .copy_from_slice(boundary);
        Ok(Self {
            state,
            length,
            boundary: retained,
            boundary_len: boundary.len(),
            owner,
        })
    }
    pub fn check_deadline(&mut self, now: Tick, work: &mut Meter) -> Result<(), Error> {
        if let Some(error) = self.state.failure {
            return Err(error);
        }
        if work.owner() != Some(self.owner) {
            return self.state.outcome(Err(Error::InvalidOwner));
        }
        self.state.check_deadline(now, work)
    }
    pub fn poll(&mut self, input: &[u8], now: Tick, work: &mut Meter) -> Result<Progress, Error> {
        self.check_deadline(now, work)?;
        let result = (|| {
            let boundary = self
                .boundary
                .get(..self.boundary_len)
                .ok_or(Error::InvalidState)?;
            self.state.poll(input, self.length, now, work, boundary)
        })();
        self.state.outcome(result)
    }
    pub fn finish(mut self, now: Tick, work: &mut Meter) -> Result<(), Error> {
        self.check_deadline(now, work)?;
        if !self.state.complete {
            return Err(Error::InvalidState);
        }
        Ok(())
    }
}
const _: () = assert!(std::mem::size_of::<Scanner>() <= 192);

/// Private resident adapter; source and original admission stay with its owner.
pub(crate) struct Core<'a> {
    source: &'a [u8],
    state: Scan,
}
impl<'a> Core<'a> {
    pub(crate) fn new(source: &'a [u8], base: u64, boundary_len: usize) -> Result<Self, Error> {
        let length = u64::try_from(source.len()).map_err(|_| Error::InvalidRange)?;
        Ok(Self {
            source,
            state: Scan::new(base, length, boundary_len)?,
        })
    }
    pub(crate) fn check_deadline(&mut self, now: Tick, work: &mut Meter) -> Result<(), Error> {
        self.state.check_deadline(now, work)
    }
    pub(crate) fn poll(
        &mut self,
        now: Tick,
        work: &mut Meter,
        boundary: &[u8],
    ) -> Result<Status, Error> {
        let result = (|| {
            let position = usize::try_from(self.state.position).map_err(|_| Error::InvalidRange)?;
            let input = self.source.get(position..).ok_or(Error::InvalidState)?;
            Ok(
                match self
                    .state
                    .poll(input, self.source.len() as u64, now, work, boundary)?
                    .status
                {
                    StreamStatus::Yield => Status::Yield,
                    StreamStatus::Delimiter(found) => Status::Delimiter(found),
                    StreamStatus::Complete => Status::Complete,
                    StreamStatus::NeedInput => return Err(Error::InvalidState),
                },
            )
        })();
        self.state.outcome(result)
    }
}
struct Scan {
    line: State,
    base: u64,
    position: u64,
    line_start: u64,
    preceding_ending: u64,
    pending_cr: bool,
    validation: usize,
    validator: Validator,
    validated: bool,
    complete: bool,
    failure: Option<Error>,
}
impl Scan {
    fn new(base: u64, length: u64, boundary_len: usize) -> Result<Self, Error> {
        base.checked_add(length).ok_or(Error::InvalidRange)?;
        Ok(Self {
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
    fn check_deadline(&mut self, now: Tick, work: &mut Meter) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = work.charge(now, Charge::default()).map_err(Error::Work);
        self.outcome(result)
    }
    fn poll(
        &mut self,
        input: &[u8],
        length: u64,
        now: Tick,
        work: &mut Meter,
        boundary: &[u8],
    ) -> Result<Progress, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.complete {
            return Ok(Progress {
                consumed: 0,
                status: StreamStatus::Complete,
            });
        }
        let result = if boundary.len() != self.line.boundary_len() {
            Err(Error::InvalidState)
        } else {
            self.step(input, length, now, work, boundary)
        };
        self.outcome(result)
    }
    fn offset(&self, position: u64) -> Result<u64, Error> {
        self.base.checked_add(position).ok_or(Error::InvalidRange)
    }
    fn end_line(&mut self, ending: u64) -> Result<Option<Delimiter>, Error> {
        let result = if let Some(found) = self.line.finish() {
            Some(Delimiter {
                line_start: self.offset(self.line_start)?,
                after_line: self.offset(self.position)?,
                // The child owner clamps this ending at its own start.
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
    fn step(
        &mut self,
        input: &[u8],
        length: u64,
        now: Tick,
        work: &mut Meter,
        boundary: &[u8],
    ) -> Result<Progress, Error> {
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
            return Ok(Progress {
                consumed: 0,
                status: StreamStatus::Yield,
            });
        }
        let mut consumed = 0;
        for _ in 0..BODY_BYTES_PER_TURN {
            if self.position == length {
                if self.line_start == self.position && !self.pending_cr {
                    self.complete = true;
                    return Ok(Progress {
                        consumed,
                        status: StreamStatus::Complete,
                    });
                }
                if self.pending_cr {
                    self.line.feed(b'\r', boundary);
                    self.pending_cr = false;
                }
                if let Some(found) = self.end_line(0)? {
                    return Ok(Progress {
                        consumed,
                        status: StreamStatus::Delimiter(found),
                    });
                }
                self.complete = true;
                return Ok(Progress {
                    consumed,
                    status: StreamStatus::Complete,
                });
            }
            if consumed == input.len() {
                return Ok(Progress {
                    consumed,
                    status: StreamStatus::NeedInput,
                });
            }
            // One source visit plus at most one selected-boundary comparison.
            work.charge(
                now,
                Charge {
                    io_bytes: 2,
                    ..Charge::default()
                },
            )
            .map_err(Error::Work)?;
            let byte = *input.get(consumed).ok_or(Error::InvalidState)?;
            self.position = self.position.checked_add(1).ok_or(Error::InvalidRange)?;
            consumed += 1;
            if byte == b'\n' {
                let ending = if self.pending_cr { 2 } else { 1 };
                if let Some(found) = self.end_line(ending)? {
                    return Ok(Progress {
                        consumed,
                        status: StreamStatus::Delimiter(found),
                    });
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
        Ok(Progress {
            consumed,
            status: StreamStatus::Yield,
        })
    }
}
const _: () = assert!(std::mem::size_of::<Cursor<'_, '_>>() <= 160);
const _: () = assert!(std::mem::size_of::<Stream<'_, '_>>() <= 160);
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::time::Deadline;
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
                assert!(cursor.core.state.validated);
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
            assert!(cursor.core.state.complete);
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
    fn stream_events(source: &[u8], boundary: &[u8], split: usize) -> (Vec<Delimiter>, Charge) {
        let mut work = meter();
        let before = work.remaining();
        let identity = std::ptr::from_ref(&work);
        let mut stream = Stream::new(17, source.len() as u64, boundary, &mut work).unwrap();
        let mut position = 0;
        let mut supplied = source.to_vec();
        supplied.extend_from_slice(b"\n--b--\r\ntail");
        let mut events = Vec::new();
        for _ in 0..source.len() * 3 + 20 {
            let end = supplied.len().min(position + split);
            let step = stream
                .poll(supplied.get(position..end).unwrap(), Tick(1))
                .unwrap();
            assert!(step.consumed <= 128 && step.consumed <= end - position);
            position += step.consumed;
            assert!(position <= source.len());
            match step.status {
                StreamStatus::Delimiter(found) => events.push(found),
                StreamStatus::Complete => {
                    assert_eq!(position, source.len());
                    let cached = stream.work.remaining();
                    assert_eq!(
                        stream.poll(b"ignored", Tick(100)).unwrap(),
                        Progress {
                            consumed: 0,
                            status: StreamStatus::Complete,
                        }
                    );
                    assert_eq!(stream.work.remaining(), cached);
                    let work = stream.finish(Tick(1)).unwrap();
                    assert_eq!(std::ptr::from_ref(work), identity);
                    return (
                        events,
                        Charge {
                            io_bytes: before.io_bytes - work.remaining().io_bytes,
                            records: before.records - work.remaining().records,
                            ..Charge::default()
                        },
                    );
                }
                StreamStatus::NeedInput => assert_eq!(position, end),
                StreamStatus::Yield => {}
            }
        }
        panic!("fragmented delimiter scan did not finish");
    }
    #[test]
    fn fragments_preserve_every_line_boundary_and_exact_source_visit_cost() {
        for source in [
            b"".as_slice(),
            b"--b",
            b"--b\r",
            b"--b\r\n--b--\r\n",
            b"\r\n\n--b\r\n\r\n--b--",
            b"\n\r\n--b\n\r\n--b--\r\n",
            b"a\r--b\n--b---tail\n--b \t\r\nepilogue",
            b"preamble\r\n--b\r\nA: B\r\n\r\nbody\r\n--b--\r\nepilogue",
            b" --b\n--boops\r\n--b \t\n--b-- \t\n",
        ] {
            for width in 1..=source.len() + 2 {
                let (events, charge) = stream_events(source, b"b", width);
                assert_eq!(events, reference(source, 17, b"b"));
                assert_eq!(charge.io_bytes, 1 + 2 * source.len() as u64);
            }
        }
        let boundary = [b'x'; 70];
        for prefix in 0..=260 {
            let mut source = vec![b'a'; prefix];
            source.extend_from_slice(b"\r\n--");
            source.extend_from_slice(&boundary);
            source.extend_from_slice(b"-- \t\r\n");
            for width in [1, 2, 3, 127, 128, 129, 4096] {
                let (events, charge) = stream_events(&source, &boundary, width);
                assert_eq!(events, reference(&source, 17, &boundary));
                assert_eq!(charge.io_bytes, 70 + 2 * source.len() as u64);
            }
        }
    }
    #[test]
    fn empty_fragments_are_not_eof_and_extent_end_does_not_consume_following_bytes() {
        let mut work = meter();
        let mut stream = Stream::new(9, 3, b"b", &mut work).unwrap();
        assert_eq!(
            stream.poll(&[], Tick(1)).unwrap().status,
            StreamStatus::Yield
        );
        let before = stream.work.remaining();
        assert_eq!(
            stream.poll(&[], Tick(1)).unwrap(),
            Progress {
                consumed: 0,
                status: StreamStatus::NeedInput,
            }
        );
        assert_eq!(stream.work.remaining().records, before.records - 1);
        assert_eq!(stream.work.remaining().io_bytes, before.io_bytes);
        assert_eq!(
            stream.poll(b"--", Tick(1)).unwrap(),
            Progress {
                consumed: 2,
                status: StreamStatus::NeedInput,
            }
        );
        assert_eq!(
            stream.poll(b"bignored", Tick(1)).unwrap(),
            Progress {
                consumed: 1,
                status: StreamStatus::Delimiter(Delimiter {
                    line_start: 9,
                    after_line: 12,
                    preceding_end: 9,
                    closing: false,
                    ignored_suffix: false,
                }),
            }
        );
        assert_eq!(
            stream.poll(b"ignored", Tick(1)).unwrap(),
            Progress {
                consumed: 0,
                status: StreamStatus::Complete,
            }
        );
        stream.finish(Tick(1)).unwrap();
        let stream = Stream::new(0, 1, b"b", &mut work).unwrap();
        assert!(matches!(stream.finish(Tick(1)), Err(Error::InvalidState)));
    }
    #[test]
    fn fragmented_original_work_and_completion_expiry_refuse_stickily() {
        let source = b"a\r\n--b\r\nz\n--b--";
        let (_, cost) = stream_events(source, b"b", 1);
        for (total, stop) in [
            (cost.io_bytes, Stop::IoBytes),
            (cost.records, Stop::Records),
        ] {
            for allowance in 0..total {
                let mut capacity = meter().remaining();
                if stop == Stop::IoBytes {
                    capacity.io_bytes = allowance;
                } else {
                    capacity.records = allowance;
                }
                let mut work = Meter::new(Deadline::after(Tick(0), 100).unwrap(), capacity);
                let mut stream = Stream::new(17, source.len() as u64, b"b", &mut work).unwrap();
                let mut position = 0;
                let error = loop {
                    let end = source.len().min(position + 1);
                    match stream.poll(source.get(position..end).unwrap(), Tick(1)) {
                        Err(error) => break error,
                        Ok(step) => {
                            assert_ne!(step.status, StreamStatus::Complete);
                            position += step.consumed;
                        }
                    }
                };
                assert_eq!(error, Error::Work(stop));
                let left = stream.work.remaining();
                assert_eq!(stream.poll(b"", Tick(1)), Err(error));
                assert_eq!(stream.check_deadline(Tick(1)), Err(error));
                assert_eq!(stream.work.remaining(), left);
                assert!(matches!(stream.finish(Tick(1)), Err(value) if value == error));
            }
        }
        for complete in [false, true] {
            let mut work = meter();
            let mut stream = Stream::new(0, 0, b"b", &mut work).unwrap();
            if complete {
                assert_eq!(
                    stream.poll(&[], Tick(1)).unwrap().status,
                    StreamStatus::Yield
                );
                assert_eq!(
                    stream.poll(&[], Tick(1)).unwrap().status,
                    StreamStatus::Complete
                );
            }
            assert!(matches!(
                stream.finish(Tick(100)),
                Err(Error::Work(Stop::Deadline))
            ));
        }
        let mut work = meter();
        assert!(matches!(
            Stream::new(u64::MAX, 1, b"b", &mut work),
            Err(Error::InvalidRange)
        ));
        assert!(matches!(
            Stream::new(0, 0, b"", &mut work),
            Err(Error::InvalidBoundary)
        ));
        let mut stream = Stream::new(0, 0, b"bad ", &mut work).unwrap();
        assert_eq!(stream.poll(&[], Tick(1)), Err(Error::InvalidBoundary));
        assert_eq!(stream.poll(&[], Tick(1)), Err(Error::InvalidBoundary));
    }
    #[test]
    fn owned_scanner_keeps_boundary_and_original_meter_across_moves() {
        let mut work = meter();
        let before = work.remaining();
        let mut boundary = *b"b";
        let mut scanner = Scanner::new(7, 9, &boundary, Tick(1), &mut work).unwrap();
        boundary.fill(b'x');
        assert_eq!(before.io_bytes - work.remaining().io_bytes, 1);
        assert_eq!(before.records - work.remaining().records, 1);
        let mut moved = work;
        let source = b"--b\n--b--";
        let mut position = 0;
        let mut events = Vec::new();
        loop {
            let end = source.len().min(position + 1);
            let step = scanner
                .poll(source.get(position..end).unwrap(), Tick(1), &mut moved)
                .unwrap();
            position += step.consumed;
            match step.status {
                StreamStatus::Delimiter(value) => events.push(value),
                StreamStatus::Complete => break,
                _ => {}
            }
        }
        assert_eq!(events, reference(source, 7, b"b"));
        scanner.finish(Tick(1), &mut moved).unwrap();
        assert_eq!(
            before.io_bytes - moved.remaining().io_bytes,
            2 + 2 * source.len() as u64
        );
    }
    #[test]
    fn owned_scanner_rejects_replacement_work_even_after_complete() {
        for complete in [false, true] {
            let mut original = meter();
            let mut scanner = Scanner::new(0, 0, b"b", Tick(1), &mut original).unwrap();
            if complete {
                assert_eq!(
                    scanner.poll(b"", Tick(1), &mut original).unwrap().status,
                    StreamStatus::Yield
                );
                assert_eq!(
                    scanner.poll(b"", Tick(1), &mut original).unwrap().status,
                    StreamStatus::Complete
                );
            }
            let before = original.remaining();
            let mut replacement = meter();
            let replacement_before = replacement.remaining();
            assert_eq!(
                scanner.poll(b"", Tick(1), &mut replacement),
                Err(Error::InvalidOwner)
            );
            assert_eq!(replacement.remaining(), replacement_before);
            assert_eq!(original.remaining(), before);
            assert_eq!(
                scanner.poll(b"", Tick(1), &mut original),
                Err(Error::InvalidOwner)
            );
            assert_eq!(
                scanner.finish(Tick(1), &mut original),
                Err(Error::InvalidOwner)
            );
        }
        for finish in [false, true] {
            let mut work = meter();
            let mut scanner = Scanner::new(0, 0, b"b", Tick(1), &mut work).unwrap();
            scanner.poll(b"", Tick(1), &mut work).unwrap();
            scanner.poll(b"", Tick(1), &mut work).unwrap();
            let error = Error::Work(Stop::Deadline);
            if !finish {
                assert_eq!(scanner.poll(b"", Tick(100), &mut work), Err(error));
            }
            assert_eq!(scanner.finish(Tick(100), &mut work), Err(error));
        }
    }
    #[test]
    fn owned_boundary_copy_is_funded_before_retention() {
        for records in [false, true] {
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: if records { 100 } else { 0 },
                    records: if records { 0 } else { 100 },
                    ..Charge::default()
                },
            );
            let stop = if records {
                Stop::Records
            } else {
                Stop::IoBytes
            };
            assert!(
                matches!(Scanner::new(0, 0, b"b", Tick(1), &mut work), Err(Error::Work(value)) if value == stop)
            );
            assert_eq!(work.stopped(), Some(stop));
        }
    }
    #[test]
    fn owned_boundary_capacity_refuses_before_debit() {
        let mut work = meter();
        let before = work.remaining();
        assert!(matches!(
            Scanner::new(0, 0, &[b'b'; 71], Tick(1), &mut work),
            Err(Error::InvalidBoundary)
        ));
        assert_eq!(work.remaining(), before);
        let mut scanner = Scanner::new(0, 0, &[b'b'; 70], Tick(1), &mut work).unwrap();
        assert_eq!(before.io_bytes - work.remaining().io_bytes, 70);
        assert_eq!(before.records - work.remaining().records, 1);
        assert_eq!(
            scanner.poll(b"", Tick(1), &mut work).unwrap().status,
            StreamStatus::Yield
        );
        assert_eq!(before.io_bytes - work.remaining().io_bytes, 140);
        assert_eq!(
            scanner.poll(b"", Tick(1), &mut work).unwrap().status,
            StreamStatus::Complete
        );
        scanner.finish(Tick(1), &mut work).unwrap();
    }
}
