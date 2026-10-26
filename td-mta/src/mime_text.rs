//! Transfer-source ownership through optional charset prescan and scalar replay.
use crate::{
    admission::work::{Charge, Meter, Stop},
    body_charset::{Plan, Prescan, Selection, Status as Scanned},
    mime_base64::Status as Bytes,
    mime_charset::{self, Charset, Decoder, Status as Decoded},
    mime_input::{self, Checkpoints},
    ports::{BlobReader, Clock, Error as PolicyError, Tick, Time},
    wire::TransferEncoding,
};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct InputSource<'a, S: BlobReader + ?Sized> {
    pub source: &'a mut S,
    pub offset: u64,
    pub length: u64,
    pub encoding: TransferEncoding,
    pub charset: Plan,
}
pub type Input<'a> = InputSource<'a, dyn BlobReader + 'a>;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Input(mime_input::Error),
    Charset(mime_charset::Error),
    Policy(PolicyError),
    Work(Stop),
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Input(_) => "text transfer operation failed",
            Self::Charset(_) => "text charset decoder failed",
            Self::Policy(_) => "text clock failed",
            Self::Work(_) => "text work budget exhausted",
            Self::InvalidState => "invalid text source state",
        })
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Scalar(char),
    Yield,
    Complete,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Phase {
    Save,
    Scan,
    Restore,
    Decode,
    Complete,
}
/// Owns the transfer cursor and its source borrow for both passes; never copied.
pub struct Reader<'r, 'b, S: BlobReader + ?Sized = dyn BlobReader + 'r> {
    input: mime_input::Reader<'r, 'b, S>,
    phase: Phase,
    scan: Prescan,
    decoder: Decoder,
    selection: Option<Selection>,
    byte: [u8; 1],
    buffered: bool,
    end: bool,
    accumulated_problem: bool,
    replayable: bool,
    last: Tick,
    failure: Option<Error>,
}
// Nested transfer operations and outer scalar turns share one watermark.
pub(crate) struct CheckedClock<'a> {
    source: &'a dyn Clock,
    last: AtomicU64,
    batch: Option<Tick>,
}
impl<'a> CheckedClock<'a> {
    pub(crate) fn new(source: &'a dyn Clock, last: Tick) -> Self {
        Self {
            source,
            last: AtomicU64::new(last.0),
            batch: None,
        }
    }
    pub(crate) fn begin_batch(&mut self) -> Result<(), PolicyError> {
        if self.batch.is_some() {
            return Err(PolicyError::Invalid);
        }
        self.batch = Some(self.monotonic()?);
        Ok(())
    }
    pub(crate) fn end_batch(&mut self) {
        self.batch = None;
    }
    pub(crate) fn last(&self) -> Tick {
        Tick(self.last.load(Ordering::Relaxed))
    }
    fn observe(&self, now: Tick) -> Result<Tick, PolicyError> {
        let prior = self.last.fetch_max(now.0, Ordering::Relaxed);
        if now.0 < prior {
            return Err(PolicyError::Invalid);
        }
        Ok(now)
    }
}
impl Clock for CheckedClock<'_> {
    fn sample(&self) -> Result<Time, PolicyError> {
        let now = self.source.sample()?;
        self.observe(now.monotonic)?;
        Ok(now)
    }
    fn monotonic(&self) -> Result<Tick, PolicyError> {
        self.observe(match self.batch {
            Some(now) => now,
            None => self.source.monotonic()?,
        })
    }
}
impl<'r, 'b> Reader<'r, 'b> {
    pub fn new(
        input: Input<'r>,
        buffer: &'b mut [u8],
        checkpoints: &'b mut Checkpoints,
    ) -> Result<Self, Error> {
        Self::new_typed(input, buffer, checkpoints)
    }
    pub fn new_replayable(
        input: Input<'r>,
        buffer: &'b mut [u8],
        checkpoints: &'b mut Checkpoints,
    ) -> Result<Self, Error> {
        Self::new_replayable_typed(input, buffer, checkpoints)
    }
}
impl<'r, 'b, S: BlobReader + ?Sized> Reader<'r, 'b, S> {
    pub(crate) fn source_mut(&mut self) -> &mut S {
        self.input.source_mut()
    }
    pub(crate) fn release(self) -> Result<mime_input::Released<'r, 'b, S>, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.phase != Phase::Complete {
            return Err(Error::InvalidState);
        }
        self.input.release().map_err(Error::Input)
    }

    pub(crate) fn new_typed(
        input: InputSource<'r, S>,
        buffer: &'b mut [u8],
        checkpoints: &'b mut Checkpoints,
    ) -> Result<Self, Error> {
        let (phase, selection, charset) = match input.charset {
            Plan::Prescan => (Phase::Save, None, Charset::Utf8),
            Plan::Selected(selection) => (Phase::Decode, Some(selection), selection.charset),
        };
        Ok(Self {
            input: mime_input::Reader::with_source_checkpoints(
                input.source,
                input.offset,
                input.length,
                input.encoding,
                buffer,
                checkpoints,
            )
            .map_err(Error::Input)?,
            phase,
            scan: Prescan::default(),
            decoder: Decoder::new(charset),
            selection,
            byte: [0; 1],
            buffered: false,
            end: false,
            accumulated_problem: false,
            replayable: false,
            last: Tick(0),
            failure: None,
        })
    }
    /// Also saves the original transfer position for bounded complete-pass replay.
    /// Source immutability/authorization and original work custody are external.
    pub(crate) fn new_replayable_typed(
        input: InputSource<'r, S>,
        buffer: &'b mut [u8],
        checkpoints: &'b mut Checkpoints,
    ) -> Result<Self, Error> {
        let mut reader = Self::new_typed(input, buffer, checkpoints)?;
        reader.replayable = true;
        // Even empty extents first fund and retain their original checkpoint.
        reader.phase = Phase::Save;
        Ok(reader)
    }
    /// Restart only a healthy, completed replayable reader. The original source,
    /// transfer checkpoint, selected charset and clock watermark survive. Caller
    /// supplies the same original meter and clock; this grants no new allowance.
    /// Failure permanently retires this reader, including premature restart.
    pub fn restart(&mut self, source_clock: &dyn Clock, meter: &mut Meter) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let clock = CheckedClock::new(source_clock, self.last);
        let result = (|| {
            let now = clock.monotonic().map_err(Error::Policy)?;
            meter.charge(now, Charge::default()).map_err(Error::Work)?;
            let result = (|| {
                if !self.replayable || self.phase != Phase::Complete {
                    return Err(Error::InvalidState);
                }
                let selection = self.selection.ok_or(Error::InvalidState)?;
                self.input
                    .restore_checkpoint(0, &clock, meter)
                    .map_err(Error::Input)?;
                self.accumulated_problem |= self.decoder.is_encoding_problem();
                self.decoder = Decoder::new(selection.charset);
                self.buffered = false;
                self.end = false;
                self.phase = Phase::Decode;
                Ok(())
            })();
            let after = clock.monotonic().map_err(Error::Policy)?;
            meter
                .charge(after, Charge::default())
                .map_err(Error::Work)?;
            result
        })();
        self.last = clock.last();
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub const fn selection(&self) -> Option<Selection> {
        if self.failure.is_some() {
            None
        } else {
            self.selection
        }
    }
    /// Provisional until Complete; combine with later body projection diagnostics.
    pub fn is_encoding_problem(&self) -> bool {
        self.accumulated_problem
            || self.decoder.is_encoding_problem()
            || self.selection.is_some_and(|s| s.is_encoding_problem)
    }
    pub const fn failure(&self) -> Option<Error> {
        self.failure
    }
    /// One source operation or scalar turn. Discard all provisional text on error.
    pub fn poll(&mut self, source_clock: &dyn Clock, meter: &mut Meter) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.phase == Phase::Complete {
            return Ok(Status::Complete);
        }
        let clock = CheckedClock::new(source_clock, self.last);
        let result = (|| {
            let now = clock.monotonic().map_err(Error::Policy)?;
            meter.charge(now, Charge::default()).map_err(Error::Work)?;
            let result = self.advance(&clock, now, meter);
            let after = clock.monotonic().map_err(Error::Policy)?;
            meter
                .charge(after, Charge::default())
                .map_err(Error::Work)?;
            result
        })();
        self.last = clock.last();
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn advance(
        &mut self,
        clock: &dyn Clock,
        now: Tick,
        meter: &mut Meter,
    ) -> Result<Status, Error> {
        match self.phase {
            Phase::Save => {
                self.input
                    .save_checkpoint(0, clock, meter)
                    .map_err(Error::Input)?;
                self.phase = if self.replayable && self.selection.is_some() {
                    Phase::Decode
                } else {
                    Phase::Scan
                };
                return Ok(Status::Yield);
            }
            Phase::Restore => {
                self.input
                    .restore_checkpoint(0, clock, meter)
                    .map_err(Error::Input)?;
                self.buffered = false;
                self.end = false;
                self.phase = Phase::Decode;
                return Ok(Status::Yield);
            }
            Phase::Complete => return Ok(Status::Complete),
            Phase::Scan | Phase::Decode => {}
        }
        if !self.buffered && !self.end {
            let step = self
                .input
                .poll(clock, meter, &mut self.byte)
                .map_err(Error::Input)?;
            if step.written > 1 || step.status == Bytes::NeedInput {
                return Err(Error::InvalidState);
            }
            self.buffered = step.written == 1;
            self.end = step.status == Bytes::Complete;
            self.accumulated_problem |= self.input.is_encoding_problem();
            return Ok(Status::Yield);
        }
        let bytes = if self.buffered {
            self.byte.as_slice()
        } else {
            &[]
        };
        let (consumed, status) = if self.phase == Phase::Scan {
            let step = self
                .scan
                .poll(bytes, self.end, now, meter)
                .map_err(Error::Charset)?;
            let status = match step.status {
                Scanned::NeedInput | Scanned::Yield => Status::Yield,
                Scanned::Complete(selection) => {
                    self.selection = Some(selection);
                    self.decoder = Decoder::new(selection.charset);
                    self.phase = Phase::Restore;
                    Status::Yield
                }
            };
            (step.consumed, status)
        } else {
            let step = self
                .decoder
                .poll(bytes, self.end, now, meter)
                .map_err(Error::Charset)?;
            let status = match step.status {
                Decoded::NeedInput => Status::Yield,
                Decoded::Scalar(value) => Status::Scalar(value),
                Decoded::Complete => {
                    self.phase = Phase::Complete;
                    Status::Complete
                }
            };
            (step.consumed, status)
        };
        if consumed > bytes.len() {
            return Err(Error::InvalidState);
        }
        if consumed != 0 {
            self.buffered = false;
        }
        Ok(status)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::ports::Deadline;
    struct Source<'a> {
        bytes: &'a [u8],
        reads: Vec<u64>,
        max: usize,
        fail: usize,
    }
    impl BlobReader for Source<'_> {
        fn len(&self) -> u64 {
            self.bytes.len() as u64
        }
        fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<usize, PolicyError> {
            self.reads.push(offset);
            if self.reads.len() == self.fail {
                return Err(PolicyError::Busy);
            }
            let bytes = &self.bytes[offset as usize..];
            let n = bytes.len().min(output.len()).min(self.max);
            output[..n].copy_from_slice(&bytes[..n]);
            Ok(n)
        }
    }
    struct TestClock {
        calls: AtomicU64,
        fault: u64,
        mode: u8,
    }
    impl TestClock {
        fn good() -> Self {
            Self {
                calls: AtomicU64::new(0),
                fault: u64::MAX,
                mode: 0,
            }
        }
    }
    impl Clock for TestClock {
        fn sample(&self) -> Result<Time, PolicyError> {
            let fail = self.calls.fetch_add(1, Ordering::Relaxed) == self.fault;
            if fail && self.mode == 2 {
                return Err(PolicyError::Busy);
            }
            Ok(Time {
                utc_ms: 0,
                monotonic: Tick(if fail {
                    if self.mode == 0 {
                        100
                    } else {
                        1
                    }
                } else {
                    2
                }),
            })
        }
    }
    fn budget(io: u64, records: u64) -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: io,
                records,
                output_bytes: 10000,
                ..Charge::default()
            },
        )
    }
    fn source(bytes: &[u8]) -> Source<'_> {
        Source {
            bytes,
            reads: Vec::new(),
            max: usize::MAX,
            fail: usize::MAX,
        }
    }
    fn input<'a>(
        source: &'a mut dyn BlobReader,
        len: usize,
        encoding: TransferEncoding,
        label: Option<&[u8]>,
    ) -> Input<'a> {
        Input {
            source,
            offset: 2,
            length: len as u64,
            encoding,
            charset: Plan::from_label(label),
        }
    }
    #[test]
    fn exact_extent_is_scanned_and_replayed_before_scalars() {
        for (bytes, encoding, label, expected, problem, charset) in [
            (
                b"xxcaf=C3=A9yy".as_slice(),
                TransferEncoding::QuotedPrintable,
                None,
                "café",
                true,
                Charset::Utf8,
            ),
            (
                b"xxcaf=C3=A9yy",
                TransferEncoding::QuotedPrintable,
                Some(b"utf-8".as_slice()),
                "café",
                false,
                Charset::Utf8,
            ),
            (
                b"xxa \tb=yy",
                TransferEncoding::QuotedPrintable,
                None,
                "a \tb",
                true,
                Charset::Ascii,
            ),
            (
                b"xxcaf\xc3\xa9yy".as_slice(),
                TransferEncoding::Identity,
                None,
                "café",
                true,
                Charset::Utf8,
            ),
            (
                b"xxabc yy",
                TransferEncoding::Identity,
                Some(b"ascii".as_slice()),
                "abc ",
                false,
                Charset::Ascii,
            ),
            (
                b"xx\xc3\xa9\xffyy",
                TransferEncoding::Identity,
                None,
                "���",
                true,
                Charset::Ascii,
            ),
            (
                b"xxw6k=yy",
                TransferEncoding::Base64,
                None,
                "é",
                true,
                Charset::Utf8,
            ),
            (
                b"xxYQ==!yy",
                TransferEncoding::Base64,
                None,
                "a",
                true,
                Charset::Ascii,
            ),
            (
                b"xxw6k=!yy",
                TransferEncoding::Base64,
                Some(b"utf-8"),
                "é",
                true,
                Charset::Utf8,
            ),
            (
                b"xx\xe9yy",
                TransferEncoding::Identity,
                Some(b"latin1"),
                "é",
                false,
                Charset::Latin1,
            ),
            (
                b"xx\x80yy",
                TransferEncoding::Identity,
                Some(b"cp1252"),
                "€",
                false,
                Charset::Windows1252,
            ),
            (
                b"xxayy",
                TransferEncoding::Identity,
                Some(b"unknown"),
                "a",
                true,
                Charset::Utf8,
            ),
            (
                b"xxyy",
                TransferEncoding::Identity,
                None,
                "",
                false,
                Charset::Ascii,
            ),
        ] {
            for capacity in [1, 2, mime_input::INPUT_BYTES] {
                let mut raw = source(bytes);
                raw.max = 3;
                let mut storage = vec![0; capacity];
                let mut checkpoints = Checkpoints::default();
                let mut reader = Reader::new(
                    input(&mut raw, bytes.len() - 4, encoding, label),
                    &mut storage,
                    &mut checkpoints,
                )
                .unwrap();
                assert!(std::mem::size_of_val(&reader) <= 512);
                let mut work = budget(10000, 10000);
                let clock = TestClock::good();
                let mut text = String::new();
                let mut done = false;
                let mut rewind_observed = false;
                for _ in 0..1000 {
                    let step = reader.poll(&clock, &mut work).unwrap();
                    if bytes == b"xxYQ==!yy" && reader.phase == Phase::Decode {
                        rewind_observed = true;
                        assert!(!reader.selection().unwrap().is_encoding_problem);
                        assert!(reader.is_encoding_problem());
                    }
                    match step {
                        Status::Scalar(c) => {
                            assert!(reader.selection().is_some());
                            text.push(c);
                        }
                        Status::Yield => {}
                        Status::Complete => {
                            done = true;
                            break;
                        }
                    }
                }
                assert!(done);
                if bytes == b"xxYQ==!yy" {
                    assert!(rewind_observed);
                }
                assert_eq!(text, expected);
                assert_eq!(reader.is_encoding_problem(), problem);
                assert_eq!(reader.selection().unwrap().charset, charset);
                let before = work.remaining();
                let calls = clock.calls.load(Ordering::Relaxed);
                assert_eq!(reader.poll(&clock, &mut work).unwrap(), Status::Complete);
                assert_eq!(work.remaining(), before);
                assert_eq!(clock.calls.load(Ordering::Relaxed), calls);
                assert!(raw
                    .reads
                    .iter()
                    .all(|p| *p >= 2 && *p < (bytes.len() - 2) as u64));
                if bytes.len() > 4 {
                    let passes = if Plan::from_label(label) == Plan::Prescan {
                        2
                    } else {
                        1
                    };
                    assert_eq!(raw.reads.iter().filter(|p| **p == 2).count(), passes);
                }
            }
        }
    }
    #[test]
    fn shared_budget_charges_both_passes_and_retires_before_replay() {
        let mut raw = source(b"xxayy");
        let mut bytes = [0; 1];
        let mut checkpoints = Checkpoints::default();
        let mut reader = Reader::new(
            input(&mut raw, 1, TransferEncoding::Identity, None),
            &mut bytes,
            &mut checkpoints,
        )
        .unwrap();
        let mut work = budget(100, 100);
        let clock = TestClock::good();
        for _ in 0..30 {
            if reader.poll(&clock, &mut work).unwrap() == Status::Complete {
                break;
            }
        }
        assert!(reader.phase == Phase::Complete);
        assert_eq!(
            (
                work.remaining().io_bytes,
                work.remaining().records,
                work.remaining().output_bytes
            ),
            (94, 96, 9998)
        );
        assert_eq!(raw.reads, vec![2, 2]);
        for records in [0, 2] {
            let mut raw = source(b"xxayy");
            let mut bytes = [0; 1];
            let mut checkpoints = Checkpoints::default();
            let mut reader = Reader::new(
                input(&mut raw, 1, TransferEncoding::Identity, None),
                &mut bytes,
                &mut checkpoints,
            )
            .unwrap();
            let mut work = budget(100, records);
            let mut failure = None;
            for _ in 0..30 {
                match reader.poll(&clock, &mut work) {
                    Err(e) => {
                        failure = Some(e);
                        break;
                    }
                    Ok(Status::Scalar(_)) => panic!("text before failed rewind"),
                    _ => {}
                }
            }
            let failure = failure.unwrap();
            let calls = clock.calls.load(Ordering::Relaxed);
            let mut fresh = budget(100, 100);
            let before = fresh.remaining();
            assert_eq!(reader.poll(&clock, &mut fresh), Err(failure));
            assert_eq!(fresh.remaining(), before);
            assert_eq!(clock.calls.load(Ordering::Relaxed), calls);
        }
    }
    #[test]
    fn every_nested_clock_fault_and_second_pass_io_failure_are_sticky() {
        let mut total = 0;
        for fault in std::iter::once(u64::MAX).chain(0..u64::MAX) {
            if fault != u64::MAX && fault >= total {
                break;
            }
            for mode in if fault == u64::MAX { 0..1 } else { 0..3 } {
                if fault == 0 && mode == 1 {
                    continue;
                }
                let mut raw = source(b"xxw6k=yy");
                let mut bytes = [0; 2];
                let mut checkpoints = Checkpoints::default();
                let mut reader = Reader::new(
                    input(&mut raw, 4, TransferEncoding::Base64, None),
                    &mut bytes,
                    &mut checkpoints,
                )
                .unwrap();
                let clock = TestClock {
                    calls: AtomicU64::new(0),
                    fault,
                    mode,
                };
                let mut work = budget(10000, 10000);
                let mut result = None;
                for _ in 0..1000 {
                    match reader.poll(&clock, &mut work) {
                        Ok(Status::Complete) => {
                            result = Some(Ok(()));
                            break;
                        }
                        Err(e) => {
                            result = Some(Err(e));
                            break;
                        }
                        _ => {}
                    }
                }
                let result = result.unwrap();
                if fault == u64::MAX {
                    assert!(result.is_ok());
                    total = clock.calls.load(Ordering::Relaxed);
                } else {
                    let error = result.unwrap_err();
                    assert!(reader.selection().is_none());
                    let calls = clock.calls.load(Ordering::Relaxed);
                    let mut fresh = budget(10000, 10000);
                    let remaining = fresh.remaining();
                    assert_eq!(reader.poll(&clock, &mut fresh), Err(error));
                    assert_eq!(fresh.remaining(), remaining);
                    assert_eq!(clock.calls.load(Ordering::Relaxed), calls);
                }
            }
        }
        assert!(total > 10);
        let mut raw = source(b"xxayy");
        raw.fail = 2;
        let mut bytes = [0; 1];
        let mut checkpoints = Checkpoints::default();
        let mut reader = Reader::new(
            input(&mut raw, 1, TransferEncoding::Identity, None),
            &mut bytes,
            &mut checkpoints,
        )
        .unwrap();
        let clock = TestClock::good();
        let mut work = budget(1000, 1000);
        let mut failure = None;
        for _ in 0..100 {
            match reader.poll(&clock, &mut work) {
                Err(e) => {
                    failure = Some(e);
                    break;
                }
                Ok(Status::Scalar(_)) => panic!("output before second pass read"),
                _ => {}
            }
        }
        let error = Error::Input(mime_input::Error::Policy(PolicyError::Busy));
        assert_eq!(failure, Some(error));
        assert_eq!(reader.poll(&clock, &mut budget(1000, 1000)), Err(error));
        assert_eq!(raw.reads, vec![2, 2]);
    }
}

#[cfg(test)]
#[path = "mime_text/replay_tests.rs"]
mod replay_tests;

#[cfg(test)]
mod clock_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::atomic::AtomicBool;
    struct Split {
        tick: AtomicU64,
        samples: AtomicU64,
        ticks: AtomicU64,
        fail: AtomicBool,
    }
    impl Split {
        fn new() -> Self {
            Self {
                tick: AtomicU64::new(10),
                samples: AtomicU64::new(0),
                ticks: AtomicU64::new(0),
                fail: AtomicBool::new(false),
            }
        }
        fn now(&self) -> Result<Tick, PolicyError> {
            if self.fail.load(Ordering::Relaxed) {
                return Err(PolicyError::Busy);
            }
            Ok(Tick(self.tick.load(Ordering::Relaxed)))
        }
    }
    impl Clock for Split {
        fn sample(&self) -> Result<Time, PolicyError> {
            self.samples.fetch_add(1, Ordering::Relaxed);
            Ok(Time {
                monotonic: self.now()?,
                utc_ms: 1234,
            })
        }
        fn monotonic(&self) -> Result<Tick, PolicyError> {
            self.ticks.fetch_add(1, Ordering::Relaxed);
            self.now()
        }
    }
    #[test]
    fn bounded_batch_reuses_ticks_and_resumes_fresh_failure_checks() {
        let source = Split::new();
        let mut clock = CheckedClock::new(&source, Tick(0));
        clock.begin_batch().unwrap();
        assert_eq!(clock.begin_batch(), Err(PolicyError::Invalid));
        source.tick.store(20, Ordering::Relaxed);
        for _ in 0..128 {
            assert_eq!(clock.monotonic(), Ok(Tick(10)));
        }
        assert_eq!(source.ticks.load(Ordering::Relaxed), 1);
        clock.end_batch();
        assert_eq!(clock.monotonic(), Ok(Tick(20)));
        clock.begin_batch().unwrap();
        source.fail.store(true, Ordering::Relaxed);
        assert_eq!(clock.monotonic(), Ok(Tick(20)));
        clock.end_batch();
        assert_eq!(clock.monotonic(), Err(PolicyError::Busy));
        source.fail.store(false, Ordering::Relaxed);
        source.tick.store(19, Ordering::Relaxed);
        assert_eq!(clock.begin_batch(), Err(PolicyError::Invalid));
        assert_eq!(clock.monotonic(), Err(PolicyError::Invalid));
    }
    #[test]
    fn mixed_clock_methods_share_the_watermark_and_preserve_utc() {
        for full_first in [false, true] {
            let source = Split::new();
            let clock = CheckedClock::new(&source, Tick(0));
            if full_first {
                assert_eq!(clock.sample().unwrap().utc_ms, 1234);
            } else {
                assert_eq!(clock.monotonic().unwrap(), Tick(10));
            }
            source.tick.store(11, Ordering::Relaxed);
            if full_first {
                assert_eq!(clock.monotonic().unwrap(), Tick(11));
            } else {
                assert_eq!(clock.sample().unwrap().monotonic, Tick(11));
            }
            assert_eq!(clock.sample().unwrap().monotonic, Tick(11));
            assert_eq!(clock.monotonic().unwrap(), Tick(11));
            source.tick.store(10, Ordering::Relaxed);
            assert_eq!(clock.sample(), Err(PolicyError::Invalid));
            assert_eq!(clock.monotonic(), Err(PolicyError::Invalid));
            assert_eq!(clock.last(), Tick(11));
        }
    }
    #[test]
    fn nested_monotonic_delegation_avoids_full_samples_and_keeps_errors() {
        let source = Split::new();
        let inner = CheckedClock::new(&source, Tick(0));
        let outer = CheckedClock::new(&inner, Tick(0));
        assert_eq!(outer.monotonic().unwrap(), Tick(10));
        assert_eq!(source.samples.load(Ordering::Relaxed), 0);
        assert_eq!(source.ticks.load(Ordering::Relaxed), 1);
        source.tick.store(20, Ordering::Relaxed);
        source.fail.store(true, Ordering::Relaxed);
        assert_eq!(outer.monotonic(), Err(PolicyError::Busy));
        assert_eq!(outer.sample(), Err(PolicyError::Busy));
        assert_eq!(outer.last(), Tick(10));
        assert_eq!(inner.last(), Tick(10));
        source.fail.store(false, Ordering::Relaxed);
        source.tick.store(9, Ordering::Relaxed);
        assert_eq!(outer.sample(), Err(PolicyError::Invalid));
        assert_eq!(outer.monotonic(), Err(PolicyError::Invalid));
    }
}
