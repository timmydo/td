//! Provisional address text; no NFC, encoded-word interpretation or authority.
mod budgeted;
use crate::{
    admission::work::{Meter, Stop},
    header_message_ids,
    ports::Tick,
};
pub use budgeted::Budgeted;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Malformed,
    NestingLimit,
    Work(Stop),
    InterpretationLimit,
    InvalidState,
}
impl From<header_message_ids::Error> for Error {
    fn from(error: header_message_ids::Error) -> Self {
        match error {
            header_message_ids::Error::Malformed => Self::Malformed,
            header_message_ids::Error::NestingLimit => Self::NestingLimit,
            header_message_ids::Error::Work(stop) => Self::Work(stop),
            header_message_ids::Error::InvalidState => Self::InvalidState,
            header_message_ids::Error::InterpretationLimit => Self::InterpretationLimit,
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => f.write_str("malformed address text"),
            Self::NestingLimit => f.write_str("address text comment nesting limit"),
            Self::Work(error) => write!(f, "address text work: {error}"),
            Self::InterpretationLimit => f.write_str("header interpretation limit"),
            Self::InvalidState => f.write_str("invalid address text cursor state"),
        }
    }
}
impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Parsed,
    Fallback,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Scalar(char),
    Complete,
}
/// Input is an admitted candidate slice; Parsed validates it before any scalar.
pub struct Cursor<'a> {
    inner: header_message_ids::project::Cursor<'a>,
    failure: Option<Error>,
}
impl<'a> Cursor<'a> {
    pub fn new(source: &'a [u8], mode: Mode) -> Self {
        let inner = match mode {
            Mode::Parsed => header_message_ids::project::Cursor::addr_spec(source),
            Mode::Fallback => header_message_ids::project::Cursor::fallback(source),
        };
        Self {
            inner,
            failure: None,
        }
    }
    /// Final only after Complete; repaired UTF-8 and noncharacters set it.
    pub const fn is_encoding_problem(&self) -> bool {
        self.inner.is_encoding_problem()
    }
    pub fn poll(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        use header_message_ids::project::Status as Inner;
        let result = match self.inner.poll(now, work) {
            Ok(Inner::Yield) => Ok(Status::Yield),
            Ok(Inner::Scalar(value)) => Ok(Status::Scalar(value)),
            Ok(Inner::Complete) => Ok(Status::Complete),
            Ok(Inner::Begin | Inner::End) => Err(Error::InvalidState),
            Err(error) => Err(error.into()),
        };
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{admission::work::Charge, ports::Deadline};
    fn work() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 20_000_000,
                records: 20_000_000,
                output_bytes: 20_000_000,
                ..Charge::default()
            },
        )
    }
    fn project(source: &[u8], mode: Mode) -> Result<(String, bool), Error> {
        let mut cursor = Cursor::new(source, mode);
        let mut meter = work();
        let mut text = String::new();
        assert!(std::mem::size_of_val(&cursor) <= 416);
        for _ in 0..2_000_000 {
            let before = meter.remaining();
            let step = cursor.poll(Tick(1), &mut meter);
            let after = meter.remaining();
            assert!(before.io_bytes - after.io_bytes <= 256);
            assert!(before.records - after.records <= 33);
            assert!(before.output_bytes - after.output_bytes <= 4);
            match step? {
                Status::Yield => {}
                Status::Scalar(value) => text.push(value),
                Status::Complete => {
                    assert_eq!(cursor.poll(Tick(100), &mut meter), Ok(Status::Complete));
                    assert_eq!(after, meter.remaining());
                    return Ok((text, cursor.is_encoding_problem()));
                }
            }
        }
        panic!("address text did not finish");
    }
    #[test]
    fn parsed_text_strips_cfws_without_changing_address_identity() {
        for (source, expected) in [
            (" (n) a . b @ EXAMPLE (tail)", "a.b@EXAMPLE"),
            ("\"a\r\n b\"@[x\n\ty]", "\"a b\"@[x\ty]"),
            ("e\u{301}@例", "e\u{301}@例"),
            ("=?utf-8?q?name?=@b", "=?utf-8?q?name?=@b"),
            ("\"\\\0\"@b", "\"\\\0\"@b"),
        ] {
            assert_eq!(
                project(source.as_bytes(), Mode::Parsed),
                Ok((expected.to_owned(), false))
            );
        }
        assert_eq!(
            project("\u{fdd0}@b".as_bytes(), Mode::Parsed),
            Ok(("�@b".to_owned(), true))
        );
        let source = format!("{}@b", "🐈".repeat(10_000));
        assert_eq!(
            project(source.as_bytes(), Mode::Parsed),
            Ok((source, false))
        );
    }
    #[test]
    fn fallback_trims_edges_unfolds_and_repairs_only_encoding_faults() {
        for (source, expected, problem) in [
            (b" \tbad\r\n addr \r\n ".as_slice(), "bad addr", false),
            (b" \t\r\n", "", false),
            (b"", "", false),
            (b"a\nb", "a\nb", false),
            (b"a\rb", "a\rb", false),
            (b"\0\x01\x7f", "\0\u{1}\u{7f}", false),
            (b"=?utf-8?q?name?=", "=?utf-8?q?name?=", false),
            (b"\xffx\xe2\x82", "�x�", true),
            (b"\xf0\x80\x80\xaf", "����", true),
            ("e\u{301}\u{fdd0}".as_bytes(), "e\u{301}�", true),
        ] {
            assert_eq!(
                project(source, Mode::Fallback),
                Ok((expected.to_owned(), problem)),
                "{source:?}"
            );
        }
        let source = format!("\t{}\r\n ", "🐈".repeat(10_000));
        assert_eq!(
            project(source.as_bytes(), Mode::Fallback),
            Ok(("🐈".repeat(10_000), false))
        );
    }
    #[test]
    fn parsed_malformed_tail_never_emits_a_scalar() {
        for source in [
            b"a@b bad".as_slice(),
            b"a@b\r",
            b"a@b\xff",
            b"<a@b>",
            b"a@b,c@d",
        ] {
            let mut cursor = Cursor::new(source, Mode::Parsed);
            let mut meter = work();
            loop {
                match cursor.poll(Tick(1), &mut meter) {
                    Ok(Status::Yield) => {}
                    Ok(_) => panic!("malformed address emitted text"),
                    Err(error) => {
                        assert_eq!(error, Error::Malformed);
                        break;
                    }
                }
            }
        }
    }
    #[test]
    fn exact_work_counts_syntax_replay_trim_intermediate_and_final_output() {
        for (source, mode, io, records, output) in [
            (b"a@b".as_slice(), Mode::Parsed, 24, 56, 6),
            (b"a@b", Mode::Fallback, 8, 12, 6),
            (b"", Mode::Fallback, 0, 1, 0),
            (b" ", Mode::Fallback, 1, 2, 0),
        ] {
            let mut cursor = Cursor::new(source, mode);
            let mut meter = work();
            let before = meter.remaining();
            while cursor.poll(Tick(1), &mut meter).unwrap() != Status::Complete {}
            let after = meter.remaining();
            assert_eq!(before.io_bytes - after.io_bytes, io, "{source:?} {mode:?}");
            assert_eq!(
                before.records - after.records,
                records,
                "{source:?} {mode:?}"
            );
            assert_eq!(
                before.output_bytes - after.output_bytes,
                output,
                "{source:?} {mode:?}"
            );
        }
    }
    #[test]
    fn resource_failures_latch_across_fresh_meters_in_both_modes() {
        for mode in [Mode::Parsed, Mode::Fallback] {
            for (io_bytes, records, output_bytes, now, expected) in [
                (0, 1000, 1000, Tick(1), Stop::IoBytes),
                (1000, 0, 1000, Tick(1), Stop::Records),
                (1000, 1000, 0, Tick(1), Stop::OutputBytes),
                (1000, 1000, 1, Tick(1), Stop::OutputBytes),
                (1000, 1000, 1000, Tick(100), Stop::Deadline),
            ] {
                let mut cursor = Cursor::new(b"a@b", mode);
                let mut limited = Meter::new(
                    Deadline::after(Tick(0), 100).unwrap(),
                    Charge {
                        io_bytes,
                        records,
                        output_bytes,
                        ..Charge::default()
                    },
                );
                loop {
                    match cursor.poll(now, &mut limited) {
                        Ok(Status::Complete) => panic!("underbudget address completed"),
                        Ok(_) => {}
                        Err(error) => {
                            assert_eq!(error, Error::Work(expected));
                            break;
                        }
                    }
                }
                let mut fresh = work();
                let before = fresh.remaining();
                assert_eq!(cursor.poll(Tick(1), &mut fresh), Err(Error::Work(expected)));
                assert_eq!(before, fresh.remaining());
            }
        }
    }
}
