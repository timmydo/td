//! Bounded raw header extents; no value normalization or source ownership.
use crate::{
    admission::work::{Meter, Stop},
    header_work::{Charge, Work},
    ports::Tick,
};

pub const STEP_TRANSITIONS: usize = 256;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Field {
    pub name_start: u64,
    pub name_end: u64,
    pub value_start: u64,
    pub value_end: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct End {
    pub body_start: u64,
    /// Recognized header bytes, including field endings but excluding separator.
    pub header_bytes: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Field(Field),
    Complete(End),
    NeedInput,
    Yield,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Progress {
    pub consumed: usize,
    pub status: Status,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    HeaderLimit,
    Offset,
    Work(Stop),
    InterpretationLimit,
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::HeaderLimit => "header byte limit exceeded",
            Self::Offset => "header source offset overflow",
            Self::Work(_) => "header work budget exhausted",
            Self::InterpretationLimit => "header interpretation limit",
            Self::InvalidState => "invalid header scanner state",
        })
    }
}
impl std::error::Error for Error {}
impl From<crate::decode_work::Error> for Error {
    fn from(error: crate::decode_work::Error) -> Self {
        match error {
            crate::decode_work::Error::Work(stop) => Self::Work(stop),
            crate::decode_work::Error::InterpretationLimit => Self::InterpretationLimit,
            crate::decode_work::Error::InvalidState => Self::InvalidState,
        }
    }
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum State {
    Line,
    Name,
    Gap,
    Value,
    BlankCr,
}
/// Offsets describe unchanged source bytes. Emitted fields remain provisional
/// until completion; the caller owns source retention and aggregate admission.
pub struct Scanner {
    start: u64,
    position: u64,
    line_start: u64,
    limit: u64,
    header_bytes: u64,
    field: Field,
    active: bool,
    cr: bool,
    eof: bool,
    state: State,
    complete: Option<End>,
    failure: Option<Error>,
}
impl Scanner {
    pub const fn new(offset: u64, header_bytes: u64) -> Self {
        Self {
            start: offset,
            position: offset,
            line_start: offset,
            limit: header_bytes,
            header_bytes: 0,
            field: Field {
                name_start: offset,
                name_end: offset,
                value_start: offset,
                value_end: offset,
            },
            active: false,
            cr: false,
            eof: false,
            state: State::Line,
            complete: None,
            failure: None,
        }
    }
    fn consume(&mut self, consumed: &mut usize) -> Result<(), Error> {
        self.position = self.position.checked_add(1).ok_or(Error::Offset)?;
        *consumed = consumed.checked_add(1).ok_or(Error::Offset)?;
        Ok(())
    }
    fn admit(&mut self) -> Result<(), Error> {
        let bytes = self.position.checked_sub(self.start).ok_or(Error::Offset)?;
        if bytes > self.limit {
            return Err(Error::HeaderLimit);
        }
        self.header_bytes = bytes;
        Ok(())
    }
    fn end(&mut self, body_start: u64) -> Status {
        let end = End {
            body_start,
            header_bytes: self.header_bytes,
        };
        self.complete = Some(end);
        Status::Complete(end)
    }
    fn emit(&mut self, now: Tick, meter: &mut impl Work) -> Result<Status, Error> {
        meter.charge(
            now,
            Charge {
                steps: 1,
                records: 1,
                ..Charge::default()
            },
        )?;
        self.active = false;
        Ok(Status::Field(self.field))
    }
    /// Resume with unconsumed bytes; last identifies actual source EOF.
    /// The caller brackets deterministic turns with fresh clock/cancellation checks.
    pub fn poll(
        &mut self,
        input: &[u8],
        last: bool,
        now: Tick,
        meter: &mut Meter,
    ) -> Result<Progress, Error> {
        self.poll_with_work(input, last, now, meter)
    }
    pub(crate) fn poll_with_work(
        &mut self,
        input: &[u8],
        last: bool,
        now: Tick,
        meter: &mut impl Work,
    ) -> Result<Progress, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if let Some(end) = self.complete {
            return Ok(Progress {
                consumed: 0,
                status: Status::Complete(end),
            });
        }
        let result = self.advance(input, last, now, meter);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn advance(
        &mut self,
        input: &[u8],
        last: bool,
        now: Tick,
        meter: &mut impl Work,
    ) -> Result<Progress, Error> {
        let mut consumed = 0;
        if self.eof {
            meter.charge(
                now,
                Charge {
                    steps: 1,
                    ..Charge::default()
                },
            )?;
            return Ok(Progress {
                consumed,
                status: self.end(self.position),
            });
        }
        for _ in 0..meter.scan_limit() {
            meter.charge(
                now,
                Charge {
                    visits: u64::from(consumed < input.len()),
                    steps: 1,
                    ..Charge::default()
                },
            )?;
            let byte = input.get(consumed).copied();
            let Some(byte) = byte else {
                if last {
                    self.eof = true;
                }
                let status = if !last {
                    Status::NeedInput
                } else if self.active {
                    self.emit(now, meter)?
                } else {
                    let body = if self.state == State::Line || self.state == State::Value {
                        self.position
                    } else {
                        self.line_start
                    };
                    self.end(body)
                };
                return Ok(Progress { consumed, status });
            };
            // A lookahead which emits a field is charged again on the next poll.
            if self.state == State::Line && self.active && !matches!(byte, b' ' | b'\t') {
                return Ok(Progress {
                    consumed,
                    status: self.emit(now, meter)?,
                });
            }
            match self.state {
                State::Line => match byte {
                    b' ' | b'\t' if self.active => {
                        self.consume(&mut consumed)?;
                        self.admit()?;
                        self.field.value_end = self.position;
                        self.cr = false;
                        self.state = State::Value;
                    }
                    b'\n' => {
                        self.consume(&mut consumed)?;
                        return Ok(Progress {
                            consumed,
                            status: self.end(self.position),
                        });
                    }
                    b'\r' => {
                        self.consume(&mut consumed)?;
                        self.state = State::BlankCr;
                    }
                    byte if ftext(byte) => {
                        self.field.name_start = self.position;
                        self.consume(&mut consumed)?;
                        self.field.name_end = self.position;
                        self.state = State::Name;
                    }
                    _ => {
                        return Ok(Progress {
                            consumed,
                            status: self.end(self.line_start),
                        })
                    }
                },
                State::Name | State::Gap => match byte {
                    b':' => {
                        self.consume(&mut consumed)?;
                        self.admit()?;
                        self.field.value_start = self.position;
                        self.field.value_end = self.position;
                        self.active = true;
                        self.cr = false;
                        self.state = State::Value;
                    }
                    b' ' | b'\t' => {
                        self.consume(&mut consumed)?;
                        self.state = State::Gap;
                    }
                    byte if self.state == State::Name && ftext(byte) => {
                        self.consume(&mut consumed)?;
                        self.field.name_end = self.position;
                    }
                    _ => {
                        return Ok(Progress {
                            consumed,
                            status: self.end(self.line_start),
                        })
                    }
                },
                State::Value => {
                    self.consume(&mut consumed)?;
                    self.admit()?;
                    if byte == b'\n' {
                        self.field.value_end = self
                            .position
                            .checked_sub(if self.cr { 2 } else { 1 })
                            .ok_or(Error::Offset)?;
                        self.line_start = self.position;
                        self.state = State::Line;
                    } else {
                        self.field.value_end = self.position;
                    }
                    self.cr = byte == b'\r';
                }
                State::BlankCr => {
                    if byte == b'\n' {
                        self.consume(&mut consumed)?;
                        return Ok(Progress {
                            consumed,
                            status: self.end(self.position),
                        });
                    }
                    return Ok(Progress {
                        consumed,
                        status: self.end(self.line_start),
                    });
                }
            }
        }
        Ok(Progress {
            consumed,
            status: Status::Yield,
        })
    }
}
fn ftext(byte: u8) -> bool {
    matches!(byte, 33..=57 | 59..=126)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{admission::work::Charge, ports::Deadline};
    fn meter() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000,
                records: 100,
                ..Charge::default()
            },
        )
    }
    fn scan(bytes: &[u8], split: usize, base: u64, limit: u64) -> Result<(Vec<Field>, End), Error> {
        let mut scanner = Scanner::new(base, limit);
        assert!(std::mem::size_of_val(&scanner) <= 128);
        let mut fields = Vec::new();
        let mut meter = meter();
        for (chunk, last) in [(&bytes[..split], false), (&bytes[split..], true)] {
            let mut used = 0;
            for _ in 0..1000 {
                let step = scanner.poll(&chunk[used..], last, Tick(1), &mut meter)?;
                assert!(step.consumed <= STEP_TRANSITIONS);
                used += step.consumed;
                match step.status {
                    Status::Field(field) => fields.push(field),
                    Status::Complete(end) => {
                        assert_eq!(
                            scanner.poll(b"ignored", true, Tick(100), &mut meter)?,
                            Progress {
                                consumed: 0,
                                status: Status::Complete(end)
                            }
                        );
                        return Ok((fields, end));
                    }
                    Status::NeedInput => {
                        assert_eq!(used, chunk.len());
                        assert!(!last);
                        break;
                    }
                    Status::Yield => {}
                }
            }
        }
        panic!("scanner did not finish");
    }
    #[test]
    fn aggregate_refuses_before_scanner_state_changes_and_reserves_emission() {
        use crate::{header_work::Aggregate, nfc::HeaderBudget};
        for (bytes, steps) in [(0, 100), (100, 0)] {
            let mut budget = HeaderBudget::new();
            let mut setup = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: u64::MAX,
                    records: u64::MAX,
                    ..Charge::default()
                },
            );
            budget
                .charge(
                    &mut setup,
                    Tick(1),
                    budget.source_bytes_remaining() - bytes,
                    budget.steps_remaining() - steps,
                    &mut 0,
                )
                .unwrap();
            let mut scanner = Scanner::new(0, 100);
            let mut work = meter();
            let before = work.remaining();
            let mut credit = 0;
            assert_eq!(
                scanner.poll_with_work(
                    b"X:a",
                    true,
                    Tick(1),
                    &mut Aggregate::new(&mut work, &mut budget, &mut credit)
                ),
                Err(Error::InterpretationLimit)
            );
            assert_eq!(scanner.position, 0);
            assert_eq!(scanner.header_bytes, 0);
            assert_eq!(scanner.field.name_end, 0);
            assert!(scanner.state == State::Line);
            assert!(!scanner.active);
            assert_eq!(credit, 0);
            assert_eq!(work.remaining(), before);
        }
        let mut input = b"X:".to_vec();
        input.extend(std::iter::repeat_n(b'a', 251));
        input.extend_from_slice(b"\nY:b");
        let mut scanner = Scanner::new(0, 1000);
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut credit = 0;
        let steps = budget.steps_remaining();
        let records = work.remaining().records;
        let progress = scanner
            .poll_with_work(
                &input,
                true,
                Tick(1),
                &mut Aggregate::new(&mut work, &mut budget, &mut credit),
            )
            .unwrap();
        assert!(matches!(progress.status, Status::Field(_)));
        assert_eq!(progress.consumed, 254);
        assert_eq!(steps - budget.steps_remaining(), 256);
        assert_eq!(records - work.remaining().records, 16);
    }
    #[test]
    fn raw_fields_folds_and_body_start_survive_every_split() {
        type Case = (
            &'static [u8],
            &'static [(&'static [u8], &'static [u8])],
            u64,
            u64,
        );
        let cases: &[Case] = &[
            (b"", &[], 0, 0),
            (b"\r\nbody", &[], 2, 0),
            (b"\nbody", &[], 1, 0),
            (b" orphan\r\n", &[], 0, 0),
            (b":bad", &[], 0, 0),
            (b"bad name:x\n", &[], 0, 0),
            (b"bad\rX:y", &[], 0, 0),
            (b"\r", &[], 0, 0),
            (b"word", &[], 0, 0),
            (b"Name \t\n", &[], 0, 0),
            (b"X:a\n\t \n\n", &[(b"X", b"a\n\t ")], 8, 7),
            (b"X:a\n b\n", &[(b"X", b"a\n b")], 7, 7),
            (b"X:", &[(b"X", b"")], 2, 2),
            (b"X:a\r", &[(b"X", b"a\r")], 4, 4),
            (b"X:a\r\n", &[(b"X", b"a")], 5, 5),
            (b"X:a\nbad\n", &[(b"X", b"a")], 4, 4),
            (b"X:a\n\rZ", &[(b"X", b"a")], 4, 4),
            (b"X:a\n\xff:x", &[(b"X", b"a")], 4, 4),
            (
                b"X \t: \0\xff\r\n\tmore\nY:z\n\nbody",
                &[(b"X", b" \0\xff\r\n\tmore"), (b"Y", b"z")],
                20,
                19,
            ),
        ];
        for &(bytes, expected, body, count) in cases {
            for split in 0..=bytes.len() {
                let (fields, end) = scan(bytes, split, 7, 1024).unwrap();
                assert_eq!(
                    end,
                    End {
                        body_start: body + 7,
                        header_bytes: count
                    },
                    "{bytes:?}, split {split}"
                );
                assert_eq!(fields.len(), expected.len());
                for (field, &(name, value)) in fields.iter().zip(expected) {
                    assert_eq!(
                        &bytes[(field.name_start - 7) as usize..(field.name_end - 7) as usize],
                        name
                    );
                    assert_eq!(
                        &bytes[(field.value_start - 7) as usize..(field.value_end - 7) as usize],
                        value
                    );
                }
            }
        }
    }
    #[test]
    fn header_limit_counts_fields_once_and_excludes_body_and_separator() {
        for split in 0..=12 {
            let bytes = b"X:a\r\n\r\nbody!";
            assert_eq!(
                scan(bytes, split, 0, 5).unwrap().1,
                End {
                    body_start: 7,
                    header_bytes: 5
                }
            );
            assert_eq!(scan(bytes, split, 0, 4), Err(Error::HeaderLimit));
        }
        let mut long = vec![b'x'; 4096];
        assert_eq!(
            scan(&long, 2048, 0, 0).unwrap(),
            (
                Vec::new(),
                End {
                    body_start: 0,
                    header_bytes: 0
                }
            )
        );
        long.push(b':');
        assert_eq!(scan(&long, 2048, 0, 4096), Err(Error::HeaderLimit));
        long.extend_from_slice(b"a\n\tmore\n");
        assert_eq!(scan(&long, 2048, 0, 4105).unwrap().1.header_bytes, 4105);
        assert_eq!(scan(&long, 2048, 0, 4104), Err(Error::HeaderLimit));
    }
    #[test]
    fn bounded_turns_charge_lookahead_records_and_keep_failures_sticky() {
        let input = [b'a'; 1024];
        let mut scanner = Scanner::new(0, 0);
        let mut work = meter();
        let before = work.remaining();
        assert_eq!(
            scanner.poll(&input, true, Tick(1), &mut work).unwrap(),
            Progress {
                consumed: 256,
                status: Status::Yield
            }
        );
        assert_eq!(before.io_bytes - work.remaining().io_bytes, 256);
        for (bytes, offset, limit, capacity, expected) in [
            (
                b"X:a".as_slice(),
                0,
                2,
                Charge {
                    io_bytes: 10,
                    records: 1,
                    ..Charge::default()
                },
                Error::HeaderLimit,
            ),
            (
                b"X:a".as_slice(),
                0,
                3,
                Charge {
                    io_bytes: 2,
                    records: 1,
                    ..Charge::default()
                },
                Error::Work(Stop::IoBytes),
            ),
            (
                b"X:a".as_slice(),
                0,
                3,
                Charge {
                    io_bytes: 10,
                    ..Charge::default()
                },
                Error::Work(Stop::Records),
            ),
            (
                b"X".as_slice(),
                u64::MAX,
                3,
                Charge {
                    io_bytes: 10,
                    records: 1,
                    ..Charge::default()
                },
                Error::Offset,
            ),
        ] {
            let mut scanner = Scanner::new(offset, limit);
            let mut work = Meter::new(Deadline::after(Tick(0), 100).unwrap(), capacity);
            assert_eq!(scanner.poll(bytes, true, Tick(1), &mut work), Err(expected));
            let remaining = work.remaining();
            assert_eq!(scanner.poll(b"", true, Tick(100), &mut work), Err(expected));
            assert_eq!(work.remaining(), remaining);
        }
        let mut scanner = Scanner::new(0, 100);
        let mut work = meter();
        assert_eq!(
            scanner
                .poll(b"X:a\nY:b\n", true, Tick(1), &mut work)
                .unwrap(),
            Progress {
                consumed: 4,
                status: Status::Field(Field {
                    name_start: 0,
                    name_end: 1,
                    value_start: 2,
                    value_end: 3
                })
            }
        );
        assert_eq!(work.remaining().io_bytes, 100_000 - 5);
        assert_eq!(work.remaining().records, 99);
        assert_eq!(
            scanner.poll(b"Y:b\n", true, Tick(100), &mut work),
            Err(Error::Work(Stop::Deadline))
        );
    }
    #[test]
    fn eof_is_retained_and_next_line_field_events_charge_records() {
        let mut scanner = Scanner::new(0, 3);
        let mut work = meter();
        let first = scanner.poll(b"X:a", true, Tick(1), &mut work).unwrap();
        assert_eq!(
            first,
            Progress {
                consumed: 3,
                status: Status::Field(Field {
                    name_start: 0,
                    name_end: 1,
                    value_start: 2,
                    value_end: 3
                })
            }
        );
        let before = work.remaining();
        assert_eq!(
            scanner.poll(b"ignored", false, Tick(1), &mut work).unwrap(),
            Progress {
                consumed: 0,
                status: Status::Complete(End {
                    body_start: 3,
                    header_bytes: 3
                })
            }
        );
        assert_eq!(work.remaining(), before);
        let mut scanner = Scanner::new(0, 100);
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100,
                ..Charge::default()
            },
        );
        assert_eq!(
            scanner.poll(b"X:a\nY:b", true, Tick(1), &mut work),
            Err(Error::Work(Stop::Records))
        );
        let before = work.remaining();
        assert_eq!(
            scanner.poll(b"Y:b", true, Tick(1), &mut work),
            Err(Error::Work(Stop::Records))
        );
        assert_eq!(work.remaining(), before);
    }
}
