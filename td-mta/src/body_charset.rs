//! Body charset policy; prescan consumes transfer-decoded bytes before replay.
pub use crate::mime_charset::Error;
use crate::{
    admission::work::Meter,
    mime_charset::{Charset, Decoder, Status as Decoded},
    ports::Tick,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Selection {
    pub charset: Charset,
    /// Combine with transfer, final decoding and projection diagnostics.
    pub is_encoding_problem: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Plan {
    Prescan,
    Selected(Selection),
}
impl Plan {
    /// The caller retains the original label; this does not trim or unquote it.
    pub fn from_label(label: Option<&[u8]>) -> Self {
        match label.map(Charset::parse) {
            None | Some(Some(Charset::Ascii)) => Self::Prescan,
            Some(charset) => Self::Selected(Selection {
                charset: charset.unwrap_or(Charset::Utf8),
                is_encoding_problem: charset.is_none(),
            }),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    NeedInput,
    Yield,
    Complete(Selection),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Progress {
    pub consumed: usize,
    pub status: Status,
}
/// A copy carries scan state, never the source binding or enclosing live meter.
#[derive(Clone, Copy)]
pub struct Prescan {
    decoder: Decoder,
    high: bool,
    complete: Option<Selection>,
    failure: Option<Error>,
}
impl Default for Prescan {
    fn default() -> Self {
        Self {
            decoder: Decoder::new(Charset::Utf8),
            high: false,
            complete: None,
            failure: None,
        }
    }
}
impl Prescan {
    /// Retain the unconsumed suffix and EOF flag through Yield. A selection is
    /// available only at Complete; the owner brackets turns with clock checks.
    pub fn poll(
        &mut self,
        input: &[u8],
        last: bool,
        now: Tick,
        meter: &mut Meter,
    ) -> Result<Progress, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if let Some(selection) = self.complete {
            return Ok(Progress {
                consumed: 0,
                status: Status::Complete(selection),
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
        meter: &mut Meter,
    ) -> Result<Progress, Error> {
        let step = self.decoder.poll(input, last, now, meter)?;
        let status = match step.status {
            Decoded::NeedInput => Status::NeedInput,
            Decoded::Scalar(value) => {
                self.high |= !value.is_ascii();
                Status::Yield
            }
            Decoded::Complete => {
                let invalid = self.decoder.is_encoding_problem();
                let promote = self.high && !invalid;
                let selection = Selection {
                    charset: if promote {
                        Charset::Utf8
                    } else {
                        Charset::Ascii
                    },
                    is_encoding_problem: promote || invalid,
                };
                self.complete = Some(selection);
                Status::Complete(selection)
            }
        };
        Ok(Progress {
            consumed: step.consumed,
            status,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{
        admission::work::{Charge, Stop},
        ports::Deadline,
    };
    fn meter(io: u64, records: u64) -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: io,
                records,
                ..Charge::default()
            },
        )
    }
    fn scan(source: &[u8], split: usize) -> Selection {
        let mut scan = Prescan::default();
        assert!(std::mem::size_of_val(&scan) <= 64);
        let mut work = meter(10000, 10000);
        for (part, last) in [(&source[..split], false), (&source[split..], true)] {
            let mut consumed = 0;
            for _ in 0..10000 {
                let step = scan
                    .poll(&part[consumed..], last, Tick(1), &mut work)
                    .unwrap();
                assert!(step.consumed <= 4);
                consumed += step.consumed;
                match step.status {
                    Status::Yield => {}
                    Status::NeedInput => {
                        assert!(!last);
                        assert_eq!(consumed, part.len());
                        break;
                    }
                    Status::Complete(selection) => {
                        assert!(last);
                        assert_eq!(consumed, part.len());
                        let before = work.remaining();
                        assert_eq!(
                            scan.poll(b"ignored", true, Tick(100), &mut work).unwrap(),
                            Progress {
                                consumed: 0,
                                status: Status::Complete(selection)
                            }
                        );
                        assert_eq!(work.remaining(), before);
                        return selection;
                    }
                }
            }
        }
        panic!("prescan did not finish");
    }
    #[test]
    fn absent_and_ascii_scan_but_other_labels_select_without_guessing() {
        for label in [
            None,
            Some(b"US-ASCII".as_slice()),
            Some(b"ascii"),
            Some(b"ANSI_X3.4-1968"),
        ] {
            assert_eq!(Plan::from_label(label), Plan::Prescan);
        }
        for (label, charset, problem) in [
            (b"utf8".as_slice(), Charset::Utf8, false),
            (b"UTF-8", Charset::Utf8, false),
            (b"ISO-8859-1", Charset::Latin1, false),
            (b"latin1", Charset::Latin1, false),
            (b"iso_8859-1", Charset::Latin1, false),
            (b"windows-1252", Charset::Windows1252, false),
            (b"CP1252", Charset::Windows1252, false),
            (b"", Charset::Utf8, true),
            (b" utf-8", Charset::Utf8, true),
            (b"shift_jis", Charset::Utf8, true),
        ] {
            assert_eq!(
                Plan::from_label(Some(label)),
                Plan::Selected(Selection {
                    charset,
                    is_encoding_problem: problem
                })
            );
        }
    }
    #[test]
    fn whole_body_validity_and_high_bytes_determine_selection_at_every_split() {
        for (bytes, charset, problem) in [
            (b"".as_slice(), Charset::Ascii, false),
            (b"hello\0\r\n\x7f", Charset::Ascii, false),
            ("Café".as_bytes(), Charset::Utf8, true),
            (
                "\u{fffd}\u{1fffe}\u{10ffff}".as_bytes(),
                Charset::Utf8,
                true,
            ),
            (b"\xe9", Charset::Ascii, true),
            (b"\xe1\x80", Charset::Ascii, true),
            (b"\xc0\x80", Charset::Ascii, true),
            (b"\xed\xa0\x80", Charset::Ascii, true),
            (b"\xf4\x90\x80\x80", Charset::Ascii, true),
            (b"\xc3\xa9\xff", Charset::Ascii, true),
            (b"\xff\xc3\xa9", Charset::Ascii, true),
            (b"\xe1\0\x80", Charset::Ascii, true),
        ] {
            for split in 0..=bytes.len() {
                assert_eq!(
                    scan(bytes, split),
                    Selection {
                        charset,
                        is_encoding_problem: problem
                    },
                    "{bytes:?} at {split}"
                );
            }
        }
    }
    #[test]
    fn scan_replay_is_charged_and_refusals_never_produce_a_selection() {
        let mut scan = Prescan::default();
        let mut work = meter(10, 10);
        let first = scan.poll(b"\xc3", false, Tick(1), &mut work).unwrap();
        assert_eq!(
            first,
            Progress {
                consumed: 1,
                status: Status::NeedInput
            }
        );
        let saved = scan;
        for _ in 0..2 {
            let step = scan.poll(b"\xa9", true, Tick(1), &mut work).unwrap();
            assert_eq!(
                step,
                Progress {
                    consumed: 1,
                    status: Status::Yield
                }
            );
            scan = saved;
        }
        assert_eq!(
            (work.remaining().io_bytes, work.remaining().records),
            (7, 8)
        );
        for (io, records, tick, expected) in [
            (0, 10, 1, Stop::IoBytes),
            (10, 0, 1, Stop::Records),
            (10, 10, 100, Stop::Deadline),
        ] {
            let mut scan = Prescan::default();
            assert_eq!(
                scan.poll(b"a", true, Tick(tick), &mut meter(io, records)),
                Err(Error::Work(expected))
            );
            let mut fresh = meter(10, 10);
            let before = fresh.remaining();
            assert_eq!(
                scan.poll(b"", true, Tick(1), &mut fresh),
                Err(Error::Work(expected))
            );
            assert_eq!(fresh.remaining(), before);
        }
        // A valid prefix cannot become a selection before the final suffix is seen.
        let mut scan = Prescan::default();
        assert_eq!(
            scan.poll(b"\xc3\xa9", false, Tick(1), &mut work)
                .unwrap()
                .status,
            Status::Yield
        );
        assert_eq!(
            scan.poll(b"", false, Tick(1), &mut work).unwrap().status,
            Status::NeedInput
        );
        assert_eq!(
            scan.poll(b"\xff", true, Tick(1), &mut work).unwrap().status,
            Status::Yield
        );
        assert_eq!(
            scan.poll(b"", true, Tick(1), &mut work).unwrap().status,
            Status::Complete(Selection {
                charset: Charset::Ascii,
                is_encoding_problem: true
            })
        );
    }
}
