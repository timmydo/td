//! Bounded decoding of the four Email/get body-value option arguments.
use crate::{
    admission::work::{Charge, Meter, Stop},
    jmap_number::Unsigned,
    ports::Tick,
};
use td_json::scan;
const MAX_BYTES: usize = 4 * crate::limits::MIB;
const TEXT: usize = 0;
const HTML: usize = 1;
const ALL: usize = 2;
const MAXIMUM: usize = 3;
const _: () = assert!((MAX_BYTES as u64) < u32::MAX as u64 - 16);
const TURN_BYTES: usize = 64;
/// Absent arguments default; explicit null is not a Boolean or UnsignedInt.
#[derive(Clone, Copy, Default)]
pub struct Arguments<'a> {
    pub fetch_text: Option<&'a str>,
    pub fetch_html: Option<&'a str>,
    pub fetch_all: Option<&'a str>,
    pub max_bytes: Option<&'a str>,
}
impl<'a> Arguments<'a> {
    fn sources(self) -> [Option<&'a str>; 4] {
        [
            self.fetch_text,
            self.fetch_html,
            self.fetch_all,
            self.max_bytes,
        ]
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Options {
    pub fetch_text: bool,
    pub fetch_html: bool,
    pub fetch_all: bool,
    /// Zero means no client-requested truncation, not unlimited server resources.
    pub max_bytes: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Work(Stop),
    Syntax,
    InvalidArgument,
    TooLarge,
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Work(error) => write!(f, "body-value options work: {error}"),
            Self::Syntax => f.write_str("invalid body-value option JSON"),
            Self::InvalidArgument => f.write_str("invalid body-value option type or value"),
            Self::TooLarge => f.write_str("body-value options exceed request byte ceiling"),
            Self::InvalidState => f.write_str("invalid body-value option decoder state"),
        }
    }
}
impl std::error::Error for Error {}
fn scan_error(error: scan::Error) -> Error {
    match error {
        scan::Error::Syntax => Error::Syntax,
        scan::Error::Depth => Error::InvalidArgument,
        scan::Error::Bytes | scan::Error::State => Error::InvalidState,
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Complete,
}
/// The caller resolves argument names/references and admits source bytes first.
pub struct Cursor<'a, 'w> {
    sources: [Option<&'a str>; 4],
    work: &'w mut Meter,
    index: usize,
    position: usize,
    scanner: Option<scan::Scanner>,
    number: Unsigned,
    first: Option<u8>,
    ended: bool,
    options: Options,
    failure: Option<Error>,
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub fn new(arguments: Arguments<'a>, work: &'w mut Meter, now: Tick) -> Result<Self, Error> {
        work.charge(now, Charge::default()).map_err(Error::Work)?;
        let sources = arguments.sources();
        let mut bytes = 0usize;
        for source in sources.iter().flatten() {
            bytes = bytes.checked_add(source.len()).ok_or(Error::TooLarge)?;
        }
        if bytes > MAX_BYTES {
            return Err(Error::TooLarge);
        }
        Ok(Self {
            sources,
            work,
            index: 0,
            position: 0,
            scanner: None,
            number: Unsigned::default(),
            first: None,
            ended: false,
            options: Options::default(),
            failure: None,
        })
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self
            .work
            .charge(now, Charge::default())
            .map_err(Error::Work);
        self.outcome(result)
    }
    /// Passive completion does not grant freshness or response publication.
    pub fn value(&self) -> Option<Options> {
        (self.failure.is_none() && self.index == self.sources.len()).then_some(self.options)
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        self.check_deadline(now)?;
        let result = self.step(now);
        self.outcome(result)
    }
    fn advance(&mut self) -> Result<Status, Error> {
        self.index = self.index.checked_add(1).ok_or(Error::InvalidState)?;
        self.position = 0;
        self.scanner = None;
        self.number = Unsigned::default();
        self.first = None;
        self.ended = false;
        Ok(if self.index == self.sources.len() {
            Status::Complete
        } else {
            Status::Yield
        })
    }
    fn step(&mut self, now: Tick) -> Result<Status, Error> {
        if self.index == self.sources.len() {
            return Ok(Status::Complete);
        }
        let source = self
            .sources
            .get(self.index)
            .copied()
            .ok_or(Error::InvalidState)?;
        let Some(source) = source else {
            self.work
                .charge(
                    now,
                    Charge {
                        records: 1,
                        ..Charge::default()
                    },
                )
                .map_err(Error::Work)?;
            return self.advance();
        };
        if self.scanner.is_none() {
            self.work
                .charge(
                    now,
                    Charge {
                        records: 1,
                        ..Charge::default()
                    },
                )
                .map_err(Error::Work)?;
            if source.is_empty() {
                return Err(Error::Syntax);
            }
            let length = u64::try_from(source.len()).map_err(|_| Error::TooLarge)?;
            self.scanner = Some(scan::Scanner::new(length, 1).map_err(scan_error)?);
            return Ok(Status::Yield);
        }
        let end = self
            .position
            .checked_add(TURN_BYTES)
            .ok_or(Error::InvalidState)?
            .min(source.len());
        let input = source
            .as_bytes()
            .get(self.position..end)
            .ok_or(Error::InvalidState)?;
        self.work
            .charge(
                now,
                Charge {
                    io_bytes: u64::try_from(input.len()).map_err(|_| Error::InvalidState)?,
                    records: 1,
                    ..Charge::default()
                },
            )
            .map_err(Error::Work)?;
        let progress = self
            .scanner
            .as_mut()
            .ok_or(Error::InvalidState)?
            .poll(input, end == source.len())
            .map_err(scan_error)?;
        if progress.consumed == 0 && progress.event.is_none() && !progress.complete {
            return Err(Error::InvalidState);
        }
        if self.index == MAXIMUM {
            for byte in input.get(..progress.consumed).ok_or(Error::InvalidState)? {
                self.number.byte(*byte);
            }
        }
        self.position = self
            .position
            .checked_add(progress.consumed)
            .ok_or(Error::InvalidState)?;
        match progress.event {
            Some(scan::Event::Start { kind, offset }) => {
                let expected = if self.index == MAXIMUM {
                    scan::Kind::Number
                } else {
                    scan::Kind::Boolean
                };
                if kind != expected {
                    return Err(Error::InvalidArgument);
                }
                let offset = usize::try_from(offset).map_err(|_| Error::InvalidState)?;
                self.first = source.as_bytes().get(offset).copied();
            }
            Some(scan::Event::End { .. }) => self.ended = true,
            Some(_) => return Err(Error::InvalidArgument),
            None => (),
        }
        if !progress.complete {
            return Ok(Status::Yield);
        }
        if self.position != source.len() || !self.ended || self.first.is_none() {
            return Err(Error::InvalidState);
        }
        let truth = self.first == Some(b't');
        match self.index {
            TEXT => self.options.fetch_text = truth,
            HTML => self.options.fetch_html = truth,
            ALL => self.options.fetch_all = truth,
            MAXIMUM => {
                self.options.max_bytes = std::mem::take(&mut self.number)
                    .finish()
                    .ok_or(Error::InvalidArgument)?
            }
            _ => return Err(Error::InvalidState),
        }
        self.advance()
    }
    pub fn finish(mut self, now: Tick) -> Result<(Options, &'w mut Meter), Error> {
        self.check_deadline(now)?;
        let options = self.value().ok_or(Error::InvalidState)?;
        Ok((options, self.work))
    }
}
const _: () = assert!(std::mem::size_of::<Cursor<'_, '_>>() <= 2 * 1024);

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use crate::ports::Deadline;
    fn meter() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000_000,
                records: 100_000_000,
                output_bytes: 1234,
                unlinks: 0,
            },
        )
    }
    fn drain(cursor: &mut Cursor<'_, '_>) -> Result<Options, Error> {
        for _ in 0..1_000_000 {
            if cursor.poll(Tick(1))? == Status::Complete {
                return cursor.value().ok_or(Error::InvalidState);
            }
        }
        panic!("options did not complete");
    }
    fn decode(arguments: Arguments<'_>) -> Result<Options, Error> {
        let mut work = meter();
        drain(&mut Cursor::new(arguments, &mut work, Tick(1))?)
    }
    #[test]
    fn omissions_flags_and_original_owner() {
        assert_eq!(decode(Arguments::default()), Ok(Options::default()));
        for bits in 0..8 {
            let values = [
                if bits & 1 != 0 { " true " } else { "false" },
                if bits & 2 != 0 { "true" } else { " false\n" },
                if bits & 4 != 0 { "\ttrue\r" } else { "false" },
            ];
            assert_eq!(
                decode(Arguments {
                    fetch_text: Some(values[0]),
                    fetch_html: Some(values[1]),
                    fetch_all: Some(values[2]),
                    max_bytes: None
                }),
                Ok(Options {
                    fetch_text: bits & 1 != 0,
                    fetch_html: bits & 2 != 0,
                    fetch_all: bits & 4 != 0,
                    max_bytes: 0
                })
            );
        }
        let mut work = meter();
        let pointer = std::ptr::from_mut(&mut work);
        let before = work.remaining();
        let mut cursor = Cursor::new(Arguments::default(), &mut work, Tick(1)).unwrap();
        assert!(cursor.value().is_none());
        drain(&mut cursor).unwrap();
        assert_eq!(cursor.poll(Tick(1)), Ok(Status::Complete));
        let (_, returned) = cursor.finish(Tick(1)).unwrap();
        assert_eq!(std::ptr::from_mut(returned), pointer);
        let after = returned.remaining();
        assert_eq!(before.records - after.records, 4);
        assert_eq!(before.io_bytes, after.io_bytes);
        assert_eq!(before.output_bytes, after.output_bytes);
    }
    #[test]
    fn exact_unsigned_numbers_across_turns() {
        for (raw, expected) in [
            ("0", 0),
            ("-0", 0),
            ("-0.0e999999", 0),
            ("1.00e2", 100),
            ("100e-2", 1),
            ("9007199254740991", 9007199254740991),
            (" 12 \n", 12),
        ] {
            assert_eq!(
                decode(Arguments {
                    max_bytes: Some(raw),
                    ..Arguments::default()
                })
                .unwrap()
                .max_bytes,
                expected,
                "{raw}"
            );
        }
        for raw in [
            "-1",
            "0.1",
            "9007199254740992",
            "1.00000000000000000000000000001",
            "1e99999999",
            "1e-9999999",
            "true",
            "null",
            "\"1\"",
            "[]",
            "{}",
        ] {
            assert_eq!(
                decode(Arguments {
                    max_bytes: Some(raw),
                    ..Arguments::default()
                }),
                Err(Error::InvalidArgument),
                "{raw}"
            );
        }
        for zeros in [63, 64, 65, 127, 128, 1024] {
            let raw = format!("1{}e-{zeros}", "0".repeat(zeros));
            assert_eq!(
                decode(Arguments {
                    max_bytes: Some(&raw),
                    ..Arguments::default()
                })
                .unwrap()
                .max_bytes,
                1
            );
        }
    }
    #[test]
    fn malformed_and_wrong_types_are_terminal() {
        for raw in ["null", "0", "\"true\"", "[]", "{}"] {
            assert_eq!(
                decode(Arguments {
                    fetch_text: Some(raw),
                    ..Arguments::default()
                }),
                Err(Error::InvalidArgument)
            );
        }
        for raw in ["", " ", "tru", "True", "true false", "true!", "true\0"] {
            let mut work = meter();
            let mut cursor = Cursor::new(
                Arguments {
                    fetch_html: Some(raw),
                    ..Arguments::default()
                },
                &mut work,
                Tick(1),
            )
            .unwrap();
            assert_eq!(drain(&mut cursor), Err(Error::Syntax), "{raw:?}");
            assert!(cursor.value().is_none());
            assert_eq!(cursor.poll(Tick(1)), Err(Error::Syntax));
            assert_eq!(cursor.check_deadline(Tick(100)), Err(Error::Syntax));
        }
        for raw in ["01", "1.", "1e", "1e+", "+1", "1 2", "1x"] {
            assert_eq!(
                decode(Arguments {
                    max_bytes: Some(raw),
                    ..Arguments::default()
                }),
                Err(Error::Syntax),
                "{raw}"
            );
        }
    }
    #[test]
    fn job_deadlines_remain_fresh_and_failures_stick() {
        let mut work = meter();
        assert!(matches!(
            Cursor::new(Arguments::default(), &mut work, Tick(100)),
            Err(Error::Work(Stop::Deadline))
        ));
        for mode in 0..4 {
            let mut work = meter();
            let mut cursor = Cursor::new(Arguments::default(), &mut work, Tick(1)).unwrap();
            if mode != 0 {
                drain(&mut cursor).unwrap();
            }
            if mode == 3 {
                assert!(matches!(
                    cursor.finish(Tick(100)),
                    Err(Error::Work(Stop::Deadline))
                ));
            } else {
                let result = if mode == 2 {
                    cursor.check_deadline(Tick(100))
                } else {
                    cursor.poll(Tick(100)).map(|_| ())
                };
                assert_eq!(result, Err(Error::Work(Stop::Deadline)));
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
            }
        }
        let mut work = meter();
        assert!(matches!(
            Cursor::new(Arguments::default(), &mut work, Tick(1))
                .unwrap()
                .finish(Tick(1)),
            Err(Error::InvalidState)
        ));
    }
    #[test]
    fn aggregate_bytes_and_original_allowance_cutoffs() {
        let raw = format!("{}true", " ".repeat(MAX_BYTES - 4));
        let mut work = meter();
        assert!(Cursor::new(
            Arguments {
                fetch_all: Some(&raw),
                ..Arguments::default()
            },
            &mut work,
            Tick(1)
        )
        .is_ok());
        assert!(matches!(
            Cursor::new(
                Arguments {
                    fetch_all: Some(&raw),
                    max_bytes: Some("0"),
                    ..Arguments::default()
                },
                &mut work,
                Tick(1)
            ),
            Err(Error::TooLarge)
        ));
        let args = Arguments {
            fetch_text: Some(" true "),
            max_bytes: Some("1e2"),
            ..Arguments::default()
        };
        let before = work.remaining();
        let mut cursor = Cursor::new(args, &mut work, Tick(1)).unwrap();
        drain(&mut cursor).unwrap();
        let (_, work) = cursor.finish(Tick(1)).unwrap();
        let cost_io = before.io_bytes - work.remaining().io_bytes;
        let cost_records = before.records - work.remaining().records;
        assert_eq!((cost_io, cost_records), (16, 10));
        for (io_bytes, records, expected) in [
            (cost_io, cost_records, None),
            (cost_io - 1, cost_records, Some(Stop::IoBytes)),
            (cost_io, cost_records - 1, Some(Stop::Records)),
        ] {
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes,
                    records,
                    ..Charge::default()
                },
            );
            let mut cursor = Cursor::new(args, &mut work, Tick(1)).unwrap();
            let result = drain(&mut cursor);
            if let Some(stop) = expected {
                assert_eq!(result, Err(Error::Work(stop)));
                assert!(cursor.value().is_none());
                assert_eq!(cursor.check_deadline(Tick(100)), Err(Error::Work(stop)));
                assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(stop)));
            } else {
                assert_eq!(result.unwrap().max_bytes, 100);
            }
        }
    }
    #[test]
    fn padded_and_split_scalars_keep_absolute_offsets_and_literal_costs() {
        for padding in 60..=68 {
            for literal in ["true", "false"] {
                let raw = format!("{}{}", " ".repeat(padding), literal);
                let options = decode(Arguments {
                    fetch_text: Some(&raw),
                    fetch_html: Some(&raw),
                    fetch_all: Some(&raw),
                    ..Arguments::default()
                })
                .unwrap();
                assert_eq!(
                    (options.fetch_text, options.fetch_html, options.fetch_all),
                    (literal == "true", literal == "true", literal == "true")
                );
            }
        }
        let maximum = format!("{}true", " ".repeat(MAX_BYTES - 4));
        assert!(
            decode(Arguments {
                fetch_text: Some(&maximum),
                ..Arguments::default()
            })
            .unwrap()
            .fetch_text
        );
        for (padding, number, expected_io, expected_records) in [
            (0, false, 7, 7),
            (63, false, 67, 7),
            (64, false, 71, 8),
            (65, false, 72, 8),
            (65, true, 68, 8),
        ] {
            let raw = format!(
                "{}{}",
                " ".repeat(padding),
                if number { "12" } else { "true" }
            );
            let args = if number {
                Arguments {
                    max_bytes: Some(&raw),
                    ..Arguments::default()
                }
            } else {
                Arguments {
                    fetch_text: Some(&raw),
                    ..Arguments::default()
                }
            };
            let mut work = meter();
            let before = work.remaining();
            let mut cursor = Cursor::new(args, &mut work, Tick(1)).unwrap();
            let value = drain(&mut cursor).unwrap();
            if number {
                assert_eq!(value.max_bytes, 12);
            } else {
                assert!(value.fetch_text);
            }
            let (_, work) = cursor.finish(Tick(1)).unwrap();
            assert_eq!(
                (
                    before.io_bytes - work.remaining().io_bytes,
                    before.records - work.remaining().records
                ),
                (expected_io, expected_records)
            );
        }
    }
    #[test]
    fn refusal_precedes_scanner_progress_and_scanner_errors_are_classified() {
        for (io_bytes, records, stop) in [(0, 100, Stop::IoBytes), (100, 1, Stop::Records)] {
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes,
                    records,
                    ..Charge::default()
                },
            );
            let mut cursor = Cursor::new(
                Arguments {
                    fetch_text: Some("true"),
                    ..Arguments::default()
                },
                &mut work,
                Tick(1),
            )
            .unwrap();
            assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
            assert_eq!(cursor.position, 0);
            assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(stop)));
            assert_eq!(cursor.position, 0);
            assert_eq!(cursor.scanner.as_ref().unwrap().offset(), 0);
            assert_eq!(cursor.check_deadline(Tick(100)), Err(Error::Work(stop)));
            assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(stop)));
        }
        assert_eq!(scan_error(scan::Error::Syntax), Error::Syntax);
        assert_eq!(scan_error(scan::Error::Depth), Error::InvalidArgument);
        assert_eq!(scan_error(scan::Error::Bytes), Error::InvalidState);
        assert_eq!(scan_error(scan::Error::State), Error::InvalidState);
    }
}
