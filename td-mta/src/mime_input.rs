//! One bounded transfer-decoding source over a caller-authorized immutable extent.
use crate::{
    admission::work::{Charge, Meter, Stop},
    mime_base64::{self, Decoder, Status},
    ports::{BlobReader, Clock, Error as PolicyError, Tick},
    wire::TransferEncoding,
};

pub const INPUT_BYTES: usize = 6 * 1024;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidRange,
    InvalidBacking,
    UnsupportedEncoding,
    Policy(PolicyError),
    Work(Stop),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidRange => "invalid encoded body extent",
            Self::InvalidBacking => "invalid transfer input backing",
            Self::UnsupportedEncoding => "transfer decoder is not implemented",
            Self::Policy(_) => "transfer input adapter failed",
            Self::Work(_) => "transfer input work budget exhausted",
        })
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Progress {
    pub written: usize,
    /// NeedInput is internal and is never returned by this source owner.
    pub status: Status,
}
/// This borrows a live body reader; extent validation does not authorize a part.
/// Caller backing is one source stage's input partition, never a whole-body buffer.
pub struct Reader<'r, 'b> {
    source: &'r mut dyn BlobReader,
    buffer: &'b mut [u8],
    end: u64,
    fetched: u64,
    decoded: u64,
    used: usize,
    available: usize,
    encoding: TransferEncoding,
    decoder: Decoder,
    last: Tick,
    failure: Option<Error>,
    complete: bool,
}
impl<'r, 'b> Reader<'r, 'b> {
    pub fn new(
        source: &'r mut dyn BlobReader,
        offset: u64,
        length: u64,
        encoding: TransferEncoding,
        buffer: &'b mut [u8],
    ) -> Result<Self, Error> {
        let end = offset.checked_add(length).ok_or(Error::InvalidRange)?;
        if end > source.len() {
            return Err(Error::InvalidRange);
        }
        if buffer.is_empty() || buffer.len() > INPUT_BYTES {
            return Err(Error::InvalidBacking);
        }
        if encoding == TransferEncoding::QuotedPrintable {
            return Err(Error::UnsupportedEncoding);
        }
        Ok(Self {
            source,
            buffer,
            end,
            fetched: offset,
            decoded: 0,
            used: 0,
            available: 0,
            encoding,
            decoder: Decoder::default(),
            last: Tick(0),
            failure: None,
            complete: false,
        })
    }
    pub const fn position(&self) -> u64 {
        self.decoded
    }
    pub const fn is_encoding_problem(&self) -> bool {
        self.decoder.is_encoding_problem()
    }
    pub const fn failure(&self) -> Option<Error> {
        self.failure
    }
    fn sample(&mut self, clock: &dyn Clock, meter: &mut Meter) -> Result<Tick, Error> {
        let now = clock.sample().map_err(Error::Policy)?.monotonic;
        if now < self.last {
            return Err(Error::Policy(PolicyError::Invalid));
        }
        self.last = now;
        meter.charge(now, Charge::default()).map_err(Error::Work)?;
        Ok(now)
    }
    /// One read of at most 6 KiB, or one decoder turn of at most 256 transitions.
    /// Clock/budget checks bracket the turn; post-work failure overrides its result.
    pub fn poll(
        &mut self,
        clock: &dyn Clock,
        meter: &mut Meter,
        output: &mut [u8],
    ) -> Result<Progress, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.complete {
            return Ok(Progress {
                written: 0,
                status: Status::Complete,
            });
        }
        let result = (|| {
            let now = self.sample(clock, meter)?;
            let result = self.advance(now, meter, output);
            self.sample(clock, meter)?;
            result
        })();
        match result {
            Err(error) => self.failure = Some(error),
            Ok(progress) => self.complete = progress.status == Status::Complete,
        }
        result
    }
    fn advance(
        &mut self,
        now: Tick,
        meter: &mut Meter,
        output: &mut [u8],
    ) -> Result<Progress, Error> {
        if self.used == self.available && self.fetched < self.end {
            let remaining = self
                .end
                .checked_sub(self.fetched)
                .ok_or(Error::InvalidRange)?;
            let count = usize::try_from(remaining.min(self.buffer.len() as u64))
                .map_err(|_| Error::InvalidRange)?;
            meter
                .charge(
                    now,
                    Charge {
                        io_bytes: count as u64,
                        ..Charge::default()
                    },
                )
                .map_err(Error::Work)?;
            let buffer = self.buffer.get_mut(..count).ok_or(Error::InvalidBacking)?;
            let read = self
                .source
                .read_at(self.fetched, buffer)
                .map_err(Error::Policy)?;
            if read == 0 || read > count {
                return Err(Error::Policy(PolicyError::Corrupt));
            }
            self.fetched = self
                .fetched
                .checked_add(read as u64)
                .ok_or(Error::InvalidRange)?;
            self.used = 0;
            self.available = read;
            return Ok(Progress {
                written: 0,
                status: Status::Yield,
            });
        }
        let input = self
            .buffer
            .get(self.used..self.available)
            .ok_or(Error::InvalidBacking)?;
        let last = self.fetched == self.end;
        let step = if self.encoding == TransferEncoding::Base64 {
            self.decoder
                .poll(input, output, last, now, meter)
                .map_err(Error::Work)?
        } else {
            let count = input
                .len()
                .min(output.len())
                .min(mime_base64::STEP_TRANSITIONS);
            meter
                .charge(
                    now,
                    Charge {
                        io_bytes: count as u64,
                        output_bytes: count as u64,
                        ..Charge::default()
                    },
                )
                .map_err(Error::Work)?;
            output
                .get_mut(..count)
                .ok_or(Error::InvalidBacking)?
                .copy_from_slice(input.get(..count).ok_or(Error::InvalidBacking)?);
            mime_base64::Progress {
                consumed: count,
                written: count,
                status: if count == input.len() && last {
                    Status::Complete
                } else if !input.is_empty() && output.is_empty() {
                    Status::NeedOutput
                } else {
                    Status::Yield
                },
            }
        };
        self.used = self
            .used
            .checked_add(step.consumed)
            .ok_or(Error::InvalidRange)?;
        self.decoded = self
            .decoded
            .checked_add(step.written as u64)
            .ok_or(Error::InvalidRange)?;
        Ok(Progress {
            written: step.written,
            status: if step.status == Status::NeedInput {
                Status::Yield
            } else {
                step.status
            },
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::ports::{Deadline, Time};
    use std::sync::atomic::{AtomicU64, Ordering};
    struct Source<'a> {
        bytes: &'a [u8],
        max: usize,
        calls: usize,
        mode: u8,
    }
    impl BlobReader for Source<'_> {
        fn len(&self) -> u64 {
            self.bytes.len() as u64
        }
        fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<usize, PolicyError> {
            self.calls += 1;
            match self.mode {
                1 => return Ok(0),
                2 => return Ok(output.len() + 1),
                3 => return Err(PolicyError::Busy),
                _ => {}
            }
            let input = self
                .bytes
                .get(offset as usize..)
                .ok_or(PolicyError::Invalid)?;
            let count = input.len().min(output.len()).min(self.max);
            output[..count].copy_from_slice(&input[..count]);
            Ok(count)
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
            let fault = self.calls.fetch_add(1, Ordering::Relaxed) == self.fault;
            if fault && self.mode == 2 {
                return Err(PolicyError::Busy);
            }
            Ok(Time {
                utc_ms: 0,
                monotonic: Tick(if fault {
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
    fn budget(bytes: u64, output: u64) -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: bytes,
                output_bytes: output,
                ..Charge::default()
            },
        )
    }
    #[test]
    fn bounded_extents_short_reads_and_decoding_preserve_exact_octets() {
        for (encoded, encoding, expected, problem) in [
            (
                b"TWFu".as_slice(),
                TransferEncoding::Base64,
                b"Man".as_slice(),
                false,
            ),
            (
                b"T!Q==Z".as_slice(),
                TransferEncoding::Base64,
                b"M".as_slice(),
                true,
            ),
            (
                b"a\x00\r\n\xff".as_slice(),
                TransferEncoding::Identity,
                b"a\x00\r\n\xff".as_slice(),
                false,
            ),
            (
                b"".as_slice(),
                TransferEncoding::Base64,
                b"".as_slice(),
                false,
            ),
        ] {
            for (capacity, max) in
                (1..=5).flat_map(|capacity| [1, usize::MAX].map(|max| (capacity, max)))
            {
                let mut bytes = b"prefix".to_vec();
                bytes.extend_from_slice(encoded);
                bytes.extend_from_slice(b"suffix");
                let mut source = Source {
                    bytes: &bytes,
                    max,
                    calls: 0,
                    mode: 0,
                };
                let mut backing = [0; 5];
                let mut reader = Reader::new(
                    &mut source,
                    6,
                    encoded.len() as u64,
                    encoding,
                    &mut backing[..capacity],
                )
                .unwrap();
                assert!(std::mem::size_of_val(&reader) <= 256);
                let clock = TestClock::good();
                let mut meter = budget(1000, 1000);
                let mut result = Vec::new();
                let mut done = false;
                for _ in 0..100 {
                    let mut output = [0; 1];
                    let step = reader.poll(&clock, &mut meter, &mut output).unwrap();
                    assert_ne!(step.status, Status::NeedInput);
                    result.extend_from_slice(&output[..step.written]);
                    if step.status == Status::Complete {
                        done = true;
                        break;
                    }
                }
                assert!(done);
                assert_eq!(result, expected);
                assert_eq!(reader.position(), expected.len() as u64);
                assert_eq!(reader.is_encoding_problem(), problem);
                assert_eq!(
                    reader.poll(&clock, &mut meter, &mut [0; 1]).unwrap().status,
                    Status::Complete
                );
                assert_eq!(source.calls, encoded.len().div_ceil(capacity.min(max)));
            }
        }
    }
    #[test]
    fn bounds_backpressure_and_work_refuse_without_unbounded_progress() {
        let mut source = Source {
            bytes: b"TWFu",
            max: usize::MAX,
            calls: 0,
            mode: 0,
        };
        for (offset, length) in [(u64::MAX, 1), (3, 2)] {
            assert!(matches!(
                Reader::new(
                    &mut source,
                    offset,
                    length,
                    TransferEncoding::Base64,
                    &mut [0; 4]
                ),
                Err(Error::InvalidRange)
            ));
        }
        assert!(Reader::new(
            &mut source,
            0,
            4,
            TransferEncoding::Base64,
            &mut [0; INPUT_BYTES]
        )
        .is_ok());
        assert!(matches!(
            Reader::new(&mut source, 0, 4, TransferEncoding::Base64, &mut []),
            Err(Error::InvalidBacking)
        ));
        assert!(matches!(
            Reader::new(
                &mut source,
                0,
                4,
                TransferEncoding::Base64,
                &mut [0; INPUT_BYTES + 1]
            ),
            Err(Error::InvalidBacking)
        ));
        assert!(matches!(
            Reader::new(
                &mut source,
                0,
                4,
                TransferEncoding::QuotedPrintable,
                &mut [0; 4]
            ),
            Err(Error::UnsupportedEncoding)
        ));
        let mut backing = [0; 4];
        let clock = TestClock::good();
        let mut reader =
            Reader::new(&mut source, 0, 4, TransferEncoding::Base64, &mut backing).unwrap();
        let mut short = budget(3, 3);
        assert_eq!(
            reader.poll(&clock, &mut short, &mut [0; 3]),
            Err(Error::Work(Stop::IoBytes))
        );
        assert_eq!(source.calls, 0);
        let mut reader =
            Reader::new(&mut source, 0, 4, TransferEncoding::Base64, &mut backing).unwrap();
        let mut exact = budget(8, 3);
        assert_eq!(
            reader.poll(&clock, &mut exact, &mut []).unwrap().status,
            Status::Yield
        );
        assert_eq!(
            reader.poll(&clock, &mut exact, &mut []).unwrap().status,
            Status::NeedOutput
        );
        let mut output = [0; 3];
        assert_eq!(
            reader.poll(&clock, &mut exact, &mut output).unwrap(),
            Progress {
                written: 3,
                status: Status::Complete
            }
        );
        assert_eq!(output, *b"Man");
        assert_eq!(exact.remaining(), Charge::default());
    }
    #[test]
    fn identity_completes_with_final_output_after_its_clock_check() {
        for length in [0, 1, 256, 257] {
            for late in [false, true] {
                let bytes = [b'x'; 257];
                let mut source = Source {
                    bytes: &bytes[..length],
                    max: usize::MAX,
                    calls: 0,
                    mode: 0,
                };
                let mut backing = [0; INPUT_BYTES];
                let mut reader = Reader::new(
                    &mut source,
                    0,
                    length as u64,
                    TransferEncoding::Identity,
                    &mut backing,
                )
                .unwrap();
                let good = TestClock::good();
                let mut meter = budget(1024, 1024);
                if length != 0 {
                    assert_eq!(
                        reader
                            .poll(&good, &mut meter, &mut [0; 256])
                            .unwrap()
                            .status,
                        Status::Yield
                    );
                }
                if length > 256 {
                    assert_eq!(
                        reader.poll(&good, &mut meter, &mut [0; 256]).unwrap(),
                        Progress {
                            written: 256,
                            status: Status::Yield
                        }
                    );
                }
                let clock = TestClock {
                    calls: AtomicU64::new(0),
                    fault: if late { 1 } else { u64::MAX },
                    mode: 0,
                };
                let mut output = [0; 256];
                let written = if length > 256 { length - 256 } else { length };
                let expected = if late {
                    Err(Error::Work(Stop::Deadline))
                } else {
                    Ok(Progress {
                        written,
                        status: Status::Complete,
                    })
                };
                assert_eq!(reader.poll(&clock, &mut meter, &mut output), expected);
                assert!(output[..written].iter().all(|byte| *byte == b'x'));
                assert_eq!(reader.position(), length as u64);
                let bad = TestClock {
                    calls: AtomicU64::new(0),
                    fault: 0,
                    mode: 2,
                };
                assert_eq!(
                    reader.poll(&bad, &mut meter, &mut []),
                    if late {
                        Err(Error::Work(Stop::Deadline))
                    } else {
                        Ok(Progress {
                            written: 0,
                            status: Status::Complete,
                        })
                    }
                );
                assert_eq!(bad.calls.load(Ordering::Relaxed), 0);
            }
        }
    }
    #[test]
    fn source_contract_failures_are_terminal_and_late_clock_failure_wins() {
        for mode in 1..=3 {
            for late in [false, true] {
                let mut source = Source {
                    bytes: b"TQ==",
                    max: 4,
                    calls: 0,
                    mode,
                };
                let mut backing = [0; 4];
                let mut reader =
                    Reader::new(&mut source, 0, 4, TransferEncoding::Base64, &mut backing).unwrap();
                let clock = TestClock {
                    calls: AtomicU64::new(0),
                    fault: if late { 1 } else { u64::MAX },
                    mode: 0,
                };
                let mut meter = budget(100, 100);
                let expected = if late {
                    Error::Work(Stop::Deadline)
                } else if mode == 3 {
                    Error::Policy(PolicyError::Busy)
                } else {
                    Error::Policy(PolicyError::Corrupt)
                };
                assert_eq!(reader.poll(&clock, &mut meter, &mut [0; 1]), Err(expected));
                let calls = clock.calls.load(Ordering::Relaxed);
                assert_eq!(reader.poll(&clock, &mut meter, &mut [0; 1]), Err(expected));
                assert_eq!(reader.failure(), Some(expected));
                assert_eq!(clock.calls.load(Ordering::Relaxed), calls);
                assert_eq!(source.calls, 1);
            }
        }
    }
    #[test]
    fn clocks_bracket_refill_and_decode_with_one_monotonic_watermark() {
        for refill in [false, true] {
            for mode in 0..3 {
                for fault in 0..2 {
                    let mut source = Source {
                        bytes: b"TQ==",
                        max: 4,
                        calls: 0,
                        mode: 0,
                    };
                    let mut backing = [0; 4];
                    let mut reader = Reader::new(
                        &mut source,
                        0,
                        4,
                        TransferEncoding::Base64,
                        if refill {
                            &mut backing[..2]
                        } else {
                            &mut backing
                        },
                    )
                    .unwrap();
                    let good = TestClock::good();
                    let mut meter = budget(100, 100);
                    reader.poll(&good, &mut meter, &mut [0; 1]).unwrap();
                    if refill {
                        reader.poll(&good, &mut meter, &mut [0; 1]).unwrap();
                    }
                    let clock = TestClock {
                        calls: AtomicU64::new(0),
                        fault,
                        mode,
                    };
                    let expected = match mode {
                        0 => Error::Work(Stop::Deadline),
                        1 => Error::Policy(PolicyError::Invalid),
                        _ => Error::Policy(PolicyError::Busy),
                    };
                    let mut output = [0; 1];
                    assert_eq!(reader.poll(&clock, &mut meter, &mut output), Err(expected));
                    assert_eq!(output, [if !refill && fault == 1 { b'M' } else { 0 }]);
                    assert_eq!(clock.calls.load(Ordering::Relaxed), fault + 1);
                    assert_eq!(reader.poll(&good, &mut meter, &mut output), Err(expected));
                }
            }
        }
    }
}
