//! Plain body-value scalars; HTML truncation and JSON serialization are separate.
use crate::{
    admission::work::{Charge, Meter, Stop},
    ports::Tick,
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Work(Stop),
    SizeOverflow,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Work(_) => "body value work budget exhausted",
            Self::SizeOverflow => "body value length overflow",
        })
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Scalar(char),
    Yield,
    NeedInput,
    Complete,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Progress {
    pub consumed: bool,
    pub status: Status,
}
#[derive(Clone, Copy)]
pub struct Plain {
    cap: u64,
    written: u64,
    cr: bool,
    eof: bool,
    truncated: bool,
    problem: bool,
    failure: Option<Error>,
}
impl Plain {
    /// Zero disables this argument's cap, not the enclosing work/space limits.
    pub const fn new(max_bytes: u64) -> Self {
        Self {
            cap: max_bytes,
            written: 0,
            cr: false,
            eof: false,
            truncated: false,
            problem: false,
            failure: None,
        }
    }
    pub const fn written_bytes(&self) -> u64 {
        self.written
    }
    /// Provisional until Complete; combine with transfer and charset diagnostics.
    pub const fn is_encoding_problem(&self) -> bool {
        self.problem
    }
    pub const fn is_truncated(&self) -> bool {
        self.truncated
    }
    /// Retain an unconsumed input/last pair. No further input follows a consumed
    /// last scalar. Bracket turns with clock checks; discard output on refusal.
    pub fn poll(
        &mut self,
        input: Option<char>,
        last: bool,
        now: Tick,
        meter: &mut Meter,
    ) -> Result<Progress, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.eof && !self.cr {
            return Ok(Progress {
                consumed: false,
                status: Status::Complete,
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
        input: Option<char>,
        last: bool,
        now: Tick,
        meter: &mut Meter,
    ) -> Result<Progress, Error> {
        let input = if self.eof { None } else { input };
        meter
            .charge(
                now,
                Charge {
                    records: u64::from(input.is_some()),
                    ..Charge::default()
                },
            )
            .map_err(Error::Work)?;
        if input.is_none() && last {
            self.eof = true;
        }
        if self.cr {
            if input.is_none() && !self.eof {
                return Ok(Progress {
                    consumed: false,
                    status: Status::NeedInput,
                });
            }
            self.cr = false;
            if input == Some('\n') {
                self.eof = last;
                return self.emit('\n', true, now, meter);
            }
            return self.emit('\r', false, now, meter);
        }
        match input {
            None => Ok(Progress {
                consumed: false,
                status: if self.eof {
                    Status::Complete
                } else {
                    Status::NeedInput
                },
            }),
            Some(value) => {
                self.eof = last;
                if value == '\r' {
                    self.cr = true;
                    Ok(Progress {
                        consumed: true,
                        status: Status::Yield,
                    })
                } else {
                    self.emit(value, true, now, meter)
                }
            }
        }
    }
    fn emit(
        &mut self,
        value: char,
        consumed: bool,
        now: Tick,
        meter: &mut Meter,
    ) -> Result<Progress, Error> {
        let value = if crate::unicode::is_noncharacter(value) {
            self.problem = true;
            '\u{fffd}'
        } else {
            value
        };
        if self.truncated {
            return Ok(Progress {
                consumed,
                status: Status::Yield,
            });
        }
        let size = value.len_utf8() as u64;
        let end = self.written.checked_add(size).ok_or(Error::SizeOverflow)?;
        if self.cap != 0 && end > self.cap {
            self.truncated = true;
            return Ok(Progress {
                consumed,
                status: Status::Yield,
            });
        }
        meter
            .charge(
                now,
                Charge {
                    output_bytes: size,
                    ..Charge::default()
                },
            )
            .map_err(Error::Work)?;
        self.written = end;
        Ok(Progress {
            consumed,
            status: Status::Scalar(value),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::ports::Deadline;
    fn budget(records: u64, output: u64) -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                records,
                output_bytes: output,
                ..Charge::default()
            },
        )
    }
    fn project(text: &str, cap: u64, last_scalar: bool) -> (String, bool, bool) {
        let mut value = Plain::new(cap);
        assert!(std::mem::size_of_val(&value) <= 64);
        let mut work = budget(10000, 10000);
        let chars: Vec<_> = text.chars().collect();
        let mut pos = 0;
        let mut out = String::new();
        for _ in 0..10000 {
            let input = chars.get(pos).copied();
            let last = pos == chars.len() || (last_scalar && pos + 1 == chars.len());
            let step = value.poll(input, last, Tick(1), &mut work).unwrap();
            if step.consumed {
                pos += 1;
            }
            match step.status {
                Status::Scalar(c) => out.push(c),
                Status::Yield | Status::NeedInput => {}
                Status::Complete => {
                    assert_eq!(pos, chars.len());
                    assert_eq!(value.written_bytes(), out.len() as u64);
                    let before = work.remaining();
                    assert_eq!(
                        value
                            .poll(Some('x'), true, Tick(100), &mut work)
                            .unwrap()
                            .status,
                        Status::Complete
                    );
                    assert_eq!(work.remaining(), before);
                    return (out, value.is_truncated(), value.is_encoding_problem());
                }
            }
        }
        panic!("body filter did not finish");
    }
    #[test]
    fn crlf_conversion_preserves_other_text_and_handles_eof_in_either_form() {
        for (source, expected, problem) in [
            ("", "", false),
            ("a\r\nb\rc\n", "a\nb\rc\n", false),
            ("\r", "\r", false),
            ("\r\r\n", "\r\n", false),
            ("\r\0\n", "\r\0\n", false),
            ("\t e\u{301} \0\u{7f}", "\t e\u{301} \0\u{7f}", false),
            ("\u{378}\u{fffd}", "\u{378}\u{fffd}", false),
            ("<b>text</b>", "<b>text</b>", false),
            ("\u{fdd0}\u{10ffff}", "��", true),
        ] {
            for last in [false, true] {
                assert_eq!(
                    project(source, 0, last),
                    (expected.to_owned(), false, problem)
                );
            }
        }
    }
    #[test]
    fn byte_caps_keep_a_scalar_prefix_and_validate_past_truncation() {
        for source in ["aé😀z", "éa", "a\r\nb\r", "ab\u{ffff}z", ""] {
            let normalized = source.replace("\r\n", "\n").replace('\u{ffff}', "�");
            for cap in 0..=normalized.len() + 1 {
                let mut expected = String::new();
                let mut truncated = false;
                for c in normalized.chars() {
                    if truncated || (cap != 0 && expected.len() + c.len_utf8() > cap) {
                        truncated = true;
                    } else {
                        expected.push(c);
                    }
                }
                for last in [false, true] {
                    assert_eq!(
                        project(source, cap as u64, last),
                        (expected.clone(), truncated, source.contains('\u{ffff}'))
                    );
                }
            }
        }
    }
    #[test]
    fn pending_cr_replay_is_charged_and_failures_are_sticky() {
        let mut value = Plain::new(0);
        let mut work = budget(10, 20);
        assert_eq!(
            value.poll(Some('\r'), false, Tick(1), &mut work).unwrap(),
            Progress {
                consumed: true,
                status: Status::Yield
            }
        );
        let saved = value;
        assert_eq!(
            value.poll(None, false, Tick(1), &mut work).unwrap().status,
            Status::NeedInput
        );
        for _ in 0..2 {
            assert_eq!(
                value.poll(Some('a'), true, Tick(1), &mut work).unwrap(),
                Progress {
                    consumed: false,
                    status: Status::Scalar('\r')
                }
            );
            value = saved;
        }
        assert_eq!(
            (work.remaining().records, work.remaining().output_bytes),
            (7, 18)
        );
        for (records, output, tick, error) in [
            (0, 10, 1, Error::Work(Stop::Records)),
            (10, 0, 1, Error::Work(Stop::OutputBytes)),
            (10, 10, 100, Error::Work(Stop::Deadline)),
        ] {
            let mut value = Plain::new(0);
            assert_eq!(
                value.poll(Some('a'), true, Tick(tick), &mut budget(records, output)),
                Err(error)
            );
            let mut fresh = budget(10, 10);
            let before = fresh.remaining();
            assert_eq!(value.poll(None, true, Tick(1), &mut fresh), Err(error));
            assert_eq!(fresh.remaining(), before);
        }
        let mut value = Plain::new(0);
        value.written = u64::MAX;
        assert_eq!(
            value.poll(Some('a'), true, Tick(1), &mut budget(10, 10)),
            Err(Error::SizeOverflow)
        );
        assert_eq!(
            value.poll(None, true, Tick(1), &mut budget(10, 10)),
            Err(Error::SizeOverflow)
        );
    }
    #[test]
    fn transfer_text_validation_continues_after_the_plain_output_cap() {
        use crate::{
            body_charset::Plan,
            mime_input::Checkpoints,
            mime_text::{Input, Reader, Status as Text},
            ports::{BlobReader, Clock, Error as PolicyError, Time},
            wire::TransferEncoding,
        };
        struct Source {
            reads: usize,
        }
        impl BlobReader for Source {
            fn len(&self) -> u64 {
                8
            }
            fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<usize, PolicyError> {
                self.reads += 1;
                output[0] = b"caf\xc3\xa9\r\n\xff"[offset as usize];
                Ok(1)
            }
        }
        struct GoodClock;
        impl Clock for GoodClock {
            fn sample(&self) -> Result<Time, PolicyError> {
                Ok(Time {
                    utc_ms: 0,
                    monotonic: Tick(1),
                })
            }
        }
        let mut raw = Source { reads: 0 };
        let mut bytes = [0; 3];
        let mut checkpoints = Checkpoints::default();
        let mut reader = Reader::new(
            Input {
                source: &mut raw,
                offset: 0,
                length: 8,
                encoding: TransferEncoding::Identity,
                charset: Plan::from_label(Some(b"utf-8")),
            },
            &mut bytes,
            &mut checkpoints,
        )
        .unwrap();
        let mut filter = Plain::new(4);
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 10000,
                records: 10000,
                output_bytes: 10000,
                ..Charge::default()
            },
        );
        let mut pending = None;
        let mut source_done = false;
        let mut done = false;
        let mut out = String::new();
        for _ in 0..1000 {
            if pending.is_none() && !source_done {
                match reader.poll(&GoodClock, &mut work).unwrap() {
                    Text::Scalar(c) => pending = Some(c),
                    Text::Yield => continue,
                    Text::Complete => source_done = true,
                }
            }
            let step = filter
                .poll(pending, source_done, Tick(1), &mut work)
                .unwrap();
            if step.consumed {
                pending = None;
            }
            match step.status {
                Status::Scalar(c) => out.push(c),
                Status::Yield | Status::NeedInput => {}
                Status::Complete => {
                    done = true;
                    break;
                }
            }
        }
        assert!(done && source_done && pending.is_none());
        assert_eq!(out, "caf");
        assert!(filter.is_truncated());
        assert_eq!(filter.written_bytes(), 3);
        assert!(reader.is_encoding_problem());
        assert!(!filter.is_encoding_problem());
        assert_eq!(raw.reads, 8);
    }
}
