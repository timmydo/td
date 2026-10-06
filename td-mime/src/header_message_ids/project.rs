//! Shared conversion engine; public MessageIds construction validates the whole field.
#[path = "project/budgeted.rs"]
mod budgeted;
use super::{Cursor as Parser, Error, Extent, Mode, Status as Parsed};
use crate::{
    charset::{Charset, Decoder as CharsetDecoder, Status as Decoded},
    decode_work::Work,
    time::Tick,
    unfold::{self, Decoder as Unfolder, Status as Unfolded},
    work::{Charge, Meter},
};
pub use budgeted::Budgeted;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Begin,
    Scalar(char),
    End,
    Complete,
}
#[derive(Clone, Copy)]
enum Phase {
    Validate,
    TrimStart,
    TrimEnd,
    Replay,
    Unfold,
    Decode,
    Complete,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Purpose {
    MessageIds(Mode),
    ContentId,
    AddrSpec,
    Fallback,
}
/// Non-Copy state retains syntax/replay progress and a fixed conversion byte.
pub struct Cursor<'a> {
    source: &'a [u8],
    purpose: Purpose,
    parser: Parser<'a>,
    phase: Phase,
    extent: Extent,
    position: usize,
    unfolder: Unfolder,
    decoder: CharsetDecoder,
    byte: Option<u8>,
    unfolded: bool,
    encoding_problem: bool,
    failure: Option<Error>,
}
impl<'a> Cursor<'a> {
    pub fn new(source: &'a [u8], mode: Mode) -> Self {
        Self {
            source,
            purpose: Purpose::MessageIds(mode),
            parser: Parser::new(source, mode),
            phase: Phase::Validate,
            extent: Extent { start: 0, end: 0 },
            position: 0,
            unfolder: Unfolder::default(),
            decoder: CharsetDecoder::new(Charset::Utf8),
            byte: None,
            unfolded: false,
            encoding_problem: false,
            failure: None,
        }
    }
    pub(crate) fn content_id(source: &'a [u8]) -> Self {
        let mut cursor = Self::new(source, Mode::Strict);
        cursor.purpose = Purpose::ContentId;
        cursor.parser = Parser::content_id(source);
        cursor
    }
    pub(crate) fn addr_spec(source: &'a [u8]) -> Self {
        let mut cursor = Self::new(source, Mode::Strict);
        cursor.purpose = Purpose::AddrSpec;
        cursor.parser = Parser::addr_spec(source);
        cursor
    }
    pub(crate) fn fallback(source: &'a [u8]) -> Self {
        let mut cursor = Self::new(source, Mode::Strict);
        cursor.purpose = Purpose::Fallback;
        cursor.extent = Extent {
            start: 0,
            end: source.len(),
        };
        cursor.phase = Phase::TrimStart;
        cursor
    }
    /// Final only after Complete; fallback mode also diagnoses repaired UTF-8.
    pub const fn is_encoding_problem(&self) -> bool {
        self.encoding_problem
    }
    pub fn poll(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        self.poll_with_work::<256>(now, work)
    }
    fn poll_with_work<const UNFOLD_TRANSITIONS: usize>(
        &mut self,
        now: Tick,
        work: &mut impl Work,
    ) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.phase, Phase::Complete) {
            return Ok(Status::Complete);
        }
        let result = self.step::<UNFOLD_TRANSITIONS>(now, work);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn step<const UNFOLD_TRANSITIONS: usize>(
        &mut self,
        now: Tick,
        work: &mut impl Work,
    ) -> Result<Status, Error> {
        work.charge(
            now,
            Charge {
                records: 1,
                ..Charge::default()
            },
        )?;
        match self.phase {
            Phase::Validate => {
                if self.parser.poll_with_work(now, work)? == Parsed::Complete {
                    self.parser = match self.purpose {
                        Purpose::MessageIds(mode) => Parser::new(self.source, mode),
                        Purpose::ContentId => Parser::content_id(self.source),
                        Purpose::AddrSpec => Parser::addr_spec(self.source),
                        Purpose::Fallback => return Err(Error::InvalidState),
                    };
                    self.phase = Phase::Replay;
                }
                Ok(Status::Yield)
            }
            Phase::TrimStart | Phase::TrimEnd => self.trim(now, work),
            Phase::Replay => match self.parser.poll_with_work(now, work)? {
                Parsed::Yield => Ok(Status::Yield),
                Parsed::Begin => Ok(Status::Begin),
                Parsed::End => Ok(Status::End),
                Parsed::Complete => {
                    self.phase = Phase::Complete;
                    Ok(Status::Complete)
                }
                Parsed::Part(extent) => {
                    if extent.start >= extent.end || extent.end > self.source.len() {
                        return Err(Error::InvalidState);
                    }
                    self.extent = extent;
                    self.position = extent.start;
                    self.unfolder = Unfolder::default();
                    self.decoder = CharsetDecoder::new(Charset::Utf8);
                    self.byte = None;
                    self.unfolded = false;
                    self.phase = Phase::Unfold;
                    Ok(Status::Yield)
                }
            },
            Phase::Unfold => self.unfold::<UNFOLD_TRANSITIONS>(now, work),
            Phase::Decode => self.decode(now, work),
            Phase::Complete => Ok(Status::Complete),
        }
    }
    fn trim(&mut self, now: Tick, work: &mut impl Work) -> Result<Status, Error> {
        if self.extent.start > self.extent.end {
            return Err(Error::InvalidState);
        }
        if self.extent.start == self.extent.end {
            self.phase = Phase::Complete;
            return Ok(Status::Complete);
        }
        let leading = matches!(self.phase, Phase::TrimStart);
        let position = if leading {
            self.extent.start
        } else {
            self.extent.end.checked_sub(1).ok_or(Error::InvalidState)?
        };
        work.charge(
            now,
            Charge {
                io_bytes: 1,
                ..Charge::default()
            },
        )?;
        let byte = self
            .source
            .get(position)
            .copied()
            .ok_or(Error::InvalidState)?;
        if matches!(byte, b' ' | b'\t' | b'\r' | b'\n') {
            if leading {
                self.extent.start = self
                    .extent
                    .start
                    .checked_add(1)
                    .ok_or(Error::InvalidState)?;
            } else {
                self.extent.end = position;
            }
        } else if leading {
            self.phase = Phase::TrimEnd;
        } else {
            self.position = self.extent.start;
            self.phase = Phase::Unfold;
        }
        Ok(Status::Yield)
    }
    fn unfold<const TRANSITIONS: usize>(
        &mut self,
        now: Tick,
        work: &mut impl Work,
    ) -> Result<Status, Error> {
        if self.byte.is_some() || self.unfolded {
            return Err(Error::InvalidState);
        }
        let input = self
            .source
            .get(self.position..self.extent.end)
            .ok_or(Error::InvalidState)?;
        let mut output = [0; 1];
        let progress = self
            .unfolder
            .poll_with_work::<TRANSITIONS>(input, &mut output, true, now, work)
            .map_err(|error| match error {
                unfold::Error::Work(stop) => Error::Work(stop),
                unfold::Error::InterpretationLimit => Error::InterpretationLimit,
                unfold::Error::InvalidState => Error::InvalidState,
            })?;
        if progress.consumed > input.len() || progress.written > 1 {
            return Err(Error::InvalidState);
        }
        self.position = self
            .position
            .checked_add(progress.consumed)
            .ok_or(Error::InvalidState)?;
        if progress.written == 1 {
            self.byte = output.first().copied();
        }
        match progress.status {
            Unfolded::Complete => {
                if self.position != self.extent.end {
                    return Err(Error::InvalidState);
                }
                self.unfolded = true;
            }
            Unfolded::NeedInput => return Err(Error::InvalidState),
            Unfolded::NeedOutput | Unfolded::Yield => {}
        }
        if self.byte.is_some() || self.unfolded {
            self.phase = Phase::Decode;
        }
        Ok(Status::Yield)
    }
    fn decode(&mut self, now: Tick, work: &mut impl Work) -> Result<Status, Error> {
        let input = self.byte.as_slice();
        let progress = self
            .decoder
            .poll_with_work(input, self.unfolded, now, work)?;
        if progress.consumed > input.len() {
            return Err(Error::InvalidState);
        }
        if self.decoder.is_encoding_problem() {
            if self.purpose != Purpose::Fallback {
                return Err(Error::InvalidState);
            }
            self.encoding_problem = true;
        }
        if progress.consumed == 1 {
            self.byte = None;
        }
        self.phase = if self.byte.is_some() || self.unfolded {
            Phase::Decode
        } else {
            Phase::Unfold
        };
        match progress.status {
            Decoded::NeedInput => {
                if self.byte.is_some() || self.unfolded {
                    return Err(Error::InvalidState);
                }
                Ok(Status::Yield)
            }
            Decoded::Complete => {
                if self.byte.is_some() || !self.unfolded {
                    return Err(Error::InvalidState);
                }
                if self.purpose == Purpose::Fallback {
                    self.phase = Phase::Complete;
                    Ok(Status::Complete)
                } else {
                    self.phase = Phase::Replay;
                    Ok(Status::Yield)
                }
            }
            Decoded::Scalar(value) => {
                let value = if crate::unicode::is_noncharacter(value) {
                    self.encoding_problem = true;
                    '\u{fffd}'
                } else {
                    value
                };
                work.charge(
                    now,
                    Charge {
                        output_bytes: value.len_utf8() as u64,
                        ..Charge::default()
                    },
                )?;
                Ok(Status::Scalar(value))
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{time::Deadline, work::Stop};
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
    fn project(source: &[u8], mode: Mode) -> Result<(Vec<String>, bool), Error> {
        let mut cursor = Cursor::new(source, mode);
        assert!(std::mem::size_of_val(&cursor) <= 384);
        let mut work = work();
        let mut result = Vec::new();
        let mut current: Option<String> = None;
        for _ in 0..1_000_000 {
            let before = work.remaining();
            let status = cursor.poll(Tick(1), &mut work);
            let after = work.remaining();
            assert!(before.io_bytes - after.io_bytes <= 256);
            assert!(before.records - after.records <= 33);
            assert!(before.output_bytes - after.output_bytes <= 4);
            match status? {
                Status::Yield => {}
                Status::Begin => {
                    assert!(current.is_none());
                    current = Some(String::new());
                }
                Status::Scalar(value) => current.as_mut().unwrap().push(value),
                Status::End => result.push(current.take().unwrap()),
                Status::Complete => {
                    assert!(current.is_none());
                    assert_eq!(cursor.poll(Tick(100), &mut work), Ok(Status::Complete));
                    assert_eq!(work.remaining(), after);
                    return Ok((result, cursor.is_encoding_problem()));
                }
            }
        }
        panic!("identifier text did not finish");
    }
    #[test]
    fn validated_text_unfolds_without_nfc_unquoting_or_encoded_word_decoding() {
        for (source, mode, expected) in [
            ("<a@b>", Mode::Strict, vec!["a@b"]),
            (
                "(outer)< a (x). b @ c .d ><é@例>",
                Mode::Strict,
                vec!["a.b@c.d", "é@例"],
            ),
            (
                "<\"a\r\n b\"@[c\n\td]>",
                Mode::Strict,
                vec!["\"a b\"@[c\td]"],
            ),
            ("<\"a\\\r\n b\"@c>", Mode::Strict, vec!["\"a\\ b\"@c"]),
            ("<\"\\\0\"@[x]>", Mode::Strict, vec!["\"\\\0\"@[x]"]),
            ("<e\u{301}@EXAMPLE>", Mode::Strict, vec!["e\u{301}@EXAMPLE"]),
            (
                "<=?utf-8?Q?name?=@b>",
                Mode::Strict,
                vec!["=?utf-8?Q?name?=@b"],
            ),
            (
                "old. <a@b> \"skip <c@d>\"",
                Mode::ObsoletePhrases,
                vec!["a@b"],
            ),
            ("", Mode::ObsoletePhrases, vec![]),
            ("only words", Mode::ObsoletePhrases, vec![]),
        ] {
            assert_eq!(
                project(source.as_bytes(), mode),
                Ok((expected.into_iter().map(str::to_owned).collect(), false)),
                "{source:?}"
            );
        }
    }
    #[test]
    fn malformed_or_nested_tail_cannot_emit_any_identifier_event() {
        let nested = format!("<a@b>{}", "(".repeat(33));
        for (source, mode, expected) in [
            (b"<a@b><bad>".as_slice(), Mode::Strict, Error::Malformed),
            (b"<a@b>(bad", Mode::Strict, Error::Malformed),
            (b"<a@b><\xff@c>", Mode::Strict, Error::Malformed),
            (nested.as_bytes(), Mode::Strict, Error::NestingLimit),
            (b"", Mode::Strict, Error::Malformed),
            (b"old <a@b> .", Mode::ObsoletePhrases, Error::Malformed),
        ] {
            let mut cursor = Cursor::new(source, mode);
            let mut work = work();
            let before = work.remaining();
            let error = loop {
                match cursor.poll(Tick(1), &mut work) {
                    Ok(Status::Yield) => {}
                    Ok(_) => panic!("emitted before complete syntax validation"),
                    Err(error) => break error,
                }
            };
            assert_eq!(error, expected);
            assert_eq!(before.output_bytes, work.remaining().output_bytes);
            let mut fresh = self::work();
            let remaining = fresh.remaining();
            assert_eq!(cursor.poll(Tick(1), &mut fresh), Err(expected));
            assert_eq!(fresh.remaining(), remaining);
        }
    }
    #[test]
    fn all_noncharacters_are_replaced_but_valid_neighbors_are_preserved() {
        for scalar in
            (0xfdd0..=0xfdef).chain((0..=16).flat_map(|p| [p * 65536 + 65534, p * 65536 + 65535]))
        {
            let value = char::from_u32(scalar).unwrap();
            let source = format!("<{value}@b>");
            assert_eq!(
                project(source.as_bytes(), Mode::Strict),
                Ok((vec!["�@b".to_owned()], true))
            );
        }
        assert_eq!(
            project(
                "<\u{fdcf}\u{fdf0}\u{1fffd}\u{20000}@b>".as_bytes(),
                Mode::Strict
            ),
            Ok((
                vec!["\u{fdcf}\u{fdf0}\u{1fffd}\u{20000}@b".to_owned()],
                false
            ))
        );
        let long = "🐈".repeat(10_000);
        assert_eq!(
            project(format!("<{long}@b>").as_bytes(), Mode::Strict),
            Ok((vec![format!("{long}@b")], false))
        );
    }
    #[test]
    fn replay_and_intermediate_conversion_bytes_are_charged_exactly() {
        for (source, bytes, records, output) in
            [("<a@b>", 34, 72, 6), ("<🐈@b>", 48, 78, 12), ("", 0, 8, 0)]
        {
            let mut cursor = Cursor::new(source.as_bytes(), Mode::ObsoletePhrases);
            let mut work = work();
            let before = work.remaining();
            for _ in 0..1000 {
                if cursor.poll(Tick(1), &mut work).unwrap() == Status::Complete {
                    break;
                }
            }
            assert_eq!(cursor.poll(Tick(1), &mut work), Ok(Status::Complete));
            let after = work.remaining();
            assert_eq!(before.io_bytes - after.io_bytes, bytes, "{source}");
            assert_eq!(before.records - after.records, records, "{source}");
            assert_eq!(before.output_bytes - after.output_bytes, output, "{source}");
        }
    }
    #[test]
    fn decoder_byte_and_record_refusals_retire_the_parent() {
        for (io_bytes, records, expected) in [(0, 100, Stop::IoBytes), (100, 1, Stop::Records)] {
            let mut cursor = Cursor::new(b"<a@b>", Mode::Strict);
            let mut work = work();
            while !matches!(cursor.phase, Phase::Decode) {
                assert_ne!(cursor.poll(Tick(1), &mut work).unwrap(), Status::Complete);
            }
            assert_eq!(cursor.byte, Some(b'a'));
            let mut limited = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes,
                    records,
                    output_bytes: 100,
                    ..Charge::default()
                },
            );
            assert_eq!(
                cursor.poll(Tick(1), &mut limited),
                Err(Error::Work(expected))
            );
            let before = work.remaining();
            assert_eq!(cursor.poll(Tick(1), &mut work), Err(Error::Work(expected)));
            assert_eq!(before, work.remaining());
        }
    }
    #[test]
    fn both_trim_phases_latch_byte_record_and_deadline_refusal() {
        for phase in [Phase::TrimStart, Phase::TrimEnd] {
            for (io_bytes, records, now, expected) in [
                (0, 1000, Tick(1), Stop::IoBytes),
                (1000, 0, Tick(1), Stop::Records),
                (1000, 1000, Tick(100), Stop::Deadline),
            ] {
                let mut cursor = Cursor::fallback(b" a@b ");
                let mut admitted = work();
                while std::mem::discriminant(&cursor.phase) != std::mem::discriminant(&phase) {
                    assert_eq!(cursor.poll(Tick(1), &mut admitted), Ok(Status::Yield));
                }
                let mut limited = Meter::new(
                    Deadline::after(Tick(0), 100).unwrap(),
                    Charge {
                        io_bytes,
                        records,
                        ..Charge::default()
                    },
                );
                assert_eq!(cursor.poll(now, &mut limited), Err(Error::Work(expected)));
                let before = admitted.remaining();
                assert_eq!(
                    cursor.poll(Tick(1), &mut admitted),
                    Err(Error::Work(expected))
                );
                assert_eq!(admitted.remaining(), before);
            }
        }
    }
    #[test]
    fn every_phase_retires_on_deadline_and_budget_refusal() {
        for phase in [Phase::Validate, Phase::Replay, Phase::Unfold, Phase::Decode] {
            let mut cursor = Cursor::new(b"<a@b>", Mode::Strict);
            let mut work = work();
            while std::mem::discriminant(&cursor.phase) != std::mem::discriminant(&phase) {
                assert_ne!(cursor.poll(Tick(1), &mut work).unwrap(), Status::Complete);
            }
            assert_eq!(
                cursor.poll(Tick(100), &mut work),
                Err(Error::Work(Stop::Deadline))
            );
            assert_eq!(
                cursor.poll(Tick(1), &mut self::work()),
                Err(Error::Work(Stop::Deadline))
            );
        }
        for (bytes, records, output, expected) in [
            (0, 1000, 1000, Stop::IoBytes),
            (1000, 0, 1000, Stop::Records),
            (1000, 1000, 0, Stop::OutputBytes),
            (1000, 1000, 1, Stop::OutputBytes),
            (20, 1000, 1000, Stop::IoBytes),
            (1000, 40, 1000, Stop::Records),
        ] {
            let mut cursor = Cursor::new(b"<a@b>", Mode::Strict);
            let mut limited = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: bytes,
                    records,
                    output_bytes: output,
                    ..Charge::default()
                },
            );
            let error = loop {
                match cursor.poll(Tick(1), &mut limited) {
                    Ok(Status::Complete) => panic!("budget accepted"),
                    Ok(_) => {}
                    Err(error) => break error,
                }
            };
            assert_eq!(error, Error::Work(expected));
            assert_eq!(cursor.poll(Tick(1), &mut work()), Err(error));
        }
    }
}
