#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::ports::Deadline;

struct Source<'a> {
    bytes: &'a [u8],
    reads: usize,
    fail_after: usize,
}
impl BlobReader for Source<'_> {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }
    fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<usize, PolicyError> {
        self.reads += 1;
        if self.reads > self.fail_after {
            return Err(PolicyError::Corrupt);
        }
        let bytes = &self.bytes[offset as usize..];
        let count = bytes.len().min(output.len());
        output[..count].copy_from_slice(&bytes[..count]);
        Ok(count)
    }
}
struct TestClock {
    tick: AtomicU64,
    calls: AtomicU64,
    fail: AtomicU64,
}
impl TestClock {
    fn new() -> Self {
        Self {
            tick: AtomicU64::new(1),
            calls: AtomicU64::new(0),
            fail: AtomicU64::new(u64::MAX),
        }
    }
}
impl Clock for TestClock {
    fn sample(&self) -> Result<Time, PolicyError> {
        let call = self.calls.fetch_add(1, Ordering::Relaxed);
        if call == self.fail.load(Ordering::Relaxed) {
            return Err(PolicyError::Busy);
        }
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(self.tick.load(Ordering::Relaxed)),
        })
    }
}
fn work() -> Meter {
    Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            records: 100000,
            io_bytes: 100000,
            output_bytes: 100000,
            ..Charge::default()
        },
    )
}
fn run(reader: &mut Reader<'_, '_>, clock: &dyn Clock, work: &mut Meter) -> Result<String, Error> {
    let mut value = String::new();
    for _ in 0..10000 {
        match reader.poll(clock, work)? {
            Status::Scalar(c) => value.push(c),
            Status::Yield => {}
            Status::Complete => return Ok(value),
        }
    }
    panic!("reader did not finish");
}
#[test]
fn repeated_complete_passes_keep_extent_selection_and_diagnostics() {
    for (body, encoding, charset, expected, problem, decoded_bytes) in [
        (
            b"caf\xc3\xa9\r\n".as_slice(),
            TransferEncoding::Identity,
            Some(b"utf-8".as_slice()),
            "café\r\n",
            false,
            7,
        ),
        (b"Y2Fmw6k=", TransferEncoding::Base64, None, "café", true, 5),
        (
            b"caf=E9",
            TransferEncoding::QuotedPrintable,
            Some(b"iso-8859-1"),
            "café",
            false,
            4,
        ),
        (
            b"a\xff",
            TransferEncoding::Identity,
            Some(b"utf-8"),
            "a�",
            true,
            2,
        ),
        (b"", TransferEncoding::Identity, None, "", false, 0),
    ] {
        for width in [1, 4, 64] {
            let bytes = [b"xx".as_slice(), body, b"yy"].concat();
            let mut source = Source {
                bytes: &bytes,
                reads: 0,
                fail_after: usize::MAX,
            };
            let mut buffer = vec![0; width];
            let mut checkpoints = Checkpoints::default();
            let mut reader = Reader::new_replayable(
                Input {
                    source: &mut source,
                    offset: 2,
                    length: body.len() as u64,
                    encoding,
                    charset: Plan::from_label(charset),
                },
                &mut buffer,
                &mut checkpoints,
            )
            .unwrap();
            let clock = TestClock::new();
            let mut work = work();
            assert_eq!(run(&mut reader, &clock, &mut work).unwrap(), expected);
            let selection = reader.selection();
            for _ in 0..2 {
                let before = work.remaining();
                let calls = clock.calls.load(Ordering::Relaxed);
                reader.restart(&clock, &mut work).unwrap();
                assert_eq!(clock.calls.load(Ordering::Relaxed) - calls, 4);
                assert_eq!(work.remaining().records, before.records - 1);
                assert_eq!(work.remaining().io_bytes, before.io_bytes);
                assert_eq!(work.remaining().output_bytes, before.output_bytes);
                assert_eq!(reader.selection(), selection);
                assert_eq!(run(&mut reader, &clock, &mut work).unwrap(), expected);
                assert_eq!(reader.is_encoding_problem(), problem);
                // One source visit and one transfer visit per encoded byte,
                // then one charset visit per decoded byte in these fixtures.
                assert_eq!(
                    before.io_bytes - work.remaining().io_bytes,
                    2 * body.len() as u64 + decoded_bytes
                );
                assert_eq!(
                    before.records - work.remaining().records,
                    1 + expected.chars().count() as u64
                );
                assert_eq!(
                    work.remaining().output_bytes,
                    before.output_bytes - decoded_bytes
                );
            }
        }
    }
}
#[test]
fn ordinary_premature_and_second_restart_retire_instead_of_rebinding() {
    for mode in 0..3 {
        let mut source = Source {
            bytes: b"a",
            reads: 0,
            fail_after: usize::MAX,
        };
        let mut buffer = [0; 1];
        let mut checkpoints = Checkpoints::default();
        let input = Input {
            source: &mut source,
            offset: 0,
            length: 1,
            encoding: TransferEncoding::Identity,
            charset: Plan::from_label(Some(b"utf-8")),
        };
        let mut reader = if mode == 0 {
            Reader::new(input, &mut buffer, &mut checkpoints)
        } else {
            Reader::new_replayable(input, &mut buffer, &mut checkpoints)
        }
        .unwrap();
        let clock = TestClock::new();
        let mut work = work();
        if mode != 1 {
            run(&mut reader, &clock, &mut work).unwrap();
        }
        if mode == 2 {
            reader.restart(&clock, &mut work).unwrap();
        }
        assert_eq!(reader.restart(&clock, &mut work), Err(Error::InvalidState));
        let calls = clock.calls.load(Ordering::Relaxed);
        assert_eq!(reader.poll(&clock, &mut work), Err(Error::InvalidState));
        assert_eq!(reader.restart(&clock, &mut work), Err(Error::InvalidState));
        assert_eq!(clock.calls.load(Ordering::Relaxed), calls);
        assert_eq!(reader.selection(), None);
    }
}
#[test]
fn every_restart_clock_cut_is_sticky_even_after_restore() {
    // Outer precheck, checkpoint pre/post, outer postcheck.
    for cut in 0..4 {
        let mut source = Source {
            bytes: b"x",
            reads: 0,
            fail_after: usize::MAX,
        };
        let mut buffer = [0; 1];
        let mut checkpoints = Checkpoints::default();
        let mut reader = Reader::new_replayable(
            Input {
                source: &mut source,
                offset: 0,
                length: 1,
                encoding: TransferEncoding::Identity,
                charset: Plan::from_label(Some(b"utf-8")),
            },
            &mut buffer,
            &mut checkpoints,
        )
        .unwrap();
        let clock = TestClock::new();
        let mut work = work();
        run(&mut reader, &clock, &mut work).unwrap();
        clock
            .fail
            .store(clock.calls.load(Ordering::Relaxed) + cut, Ordering::Relaxed);
        let error = reader.restart(&clock, &mut work).unwrap_err();
        assert_eq!(
            error,
            if cut == 0 || cut == 3 {
                Error::Policy(PolicyError::Busy)
            } else {
                Error::Input(mime_input::Error::Policy(PolicyError::Busy))
            }
        );
        assert_eq!(reader.failure(), Some(error));
        assert_eq!(reader.selection(), None);
        let calls = clock.calls.load(Ordering::Relaxed);
        assert_eq!(reader.restart(&clock, &mut work), Err(error));
        assert_eq!(reader.poll(&clock, &mut work), Err(error));
        assert_eq!(clock.calls.load(Ordering::Relaxed), calls);
    }
}
#[test]
fn replay_preserves_deadline_and_refuses_late_source_failure() {
    for mode in 0..4 {
        let mut source = Source {
            bytes: b"x",
            reads: 0,
            fail_after: 1,
        };
        let mut buffer = [0; 1];
        let mut checkpoints = Checkpoints::default();
        let mut reader = Reader::new_replayable(
            Input {
                source: &mut source,
                offset: 0,
                length: 1,
                encoding: TransferEncoding::Identity,
                charset: Plan::from_label(Some(b"utf-8")),
            },
            &mut buffer,
            &mut checkpoints,
        )
        .unwrap();
        let clock = TestClock::new();
        let mut work = work();
        run(&mut reader, &clock, &mut work).unwrap();
        let error = if mode == 3 {
            reader.restart(&clock, &mut work).unwrap();
            clock.tick.store(100, Ordering::Relaxed);
            run(&mut reader, &clock, &mut work).unwrap_err()
        } else if mode == 0 {
            reader.restart(&clock, &mut work).unwrap();
            run(&mut reader, &clock, &mut work).unwrap_err()
        } else {
            clock
                .tick
                .store(if mode == 1 { 100 } else { 0 }, Ordering::Relaxed);
            reader.restart(&clock, &mut work).unwrap_err()
        };
        assert_eq!(
            error,
            match mode {
                0 => Error::Input(mime_input::Error::Policy(PolicyError::Corrupt)),
                1 | 3 => Error::Work(Stop::Deadline),
                _ => Error::Policy(PolicyError::Invalid),
            }
        );
        clock.tick.store(1, Ordering::Relaxed);
        assert_eq!(reader.poll(&clock, &mut work), Err(error));
    }
}

#[test]
fn invalid_state_still_gets_post_admission_and_original_records_are_finite() {
    for mode in 0..2 {
        let mut source = Source {
            bytes: b"a",
            reads: 0,
            fail_after: usize::MAX,
        };
        let mut buffer = [0; 1];
        let mut checkpoints = Checkpoints::default();
        let mut reader = Reader::new_replayable(
            Input {
                source: &mut source,
                offset: 0,
                length: 1,
                encoding: TransferEncoding::Identity,
                charset: Plan::from_label(Some(b"utf-8")),
            },
            &mut buffer,
            &mut checkpoints,
        )
        .unwrap();
        let clock = TestClock::new();
        let mut work = work();
        let error = if mode == 0 {
            clock.fail.store(1, Ordering::Relaxed);
            Error::Policy(PolicyError::Busy)
        } else {
            run(&mut reader, &clock, &mut work).unwrap();
            let remaining = work.remaining().records;
            work.charge(
                Tick(1),
                Charge {
                    records: remaining,
                    ..Charge::default()
                },
            )
            .unwrap();
            Error::Work(Stop::Records)
        };
        assert_eq!(reader.restart(&clock, &mut work), Err(error));
        assert_eq!(reader.poll(&clock, &mut work), Err(error));
        assert_eq!(reader.restart(&clock, &mut work), Err(error));
    }
}
