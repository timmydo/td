//! Whole-field validation precedes identifier text; response publication is external.
use super::{Cursor as Parser, Error, Extent, Mode, Status as Parsed};
use crate::{
    admission::work::{Charge, Meter},
    mime_charset::{self, Charset, Decoder as CharsetDecoder, Status as Decoded},
    mime_unfold::{Decoder as Unfolder, Status as Unfolded},
    ports::Tick,
};
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
    Replay,
    Unfold,
    Decode,
    Complete,
}
/// Non-Copy state retains both syntax passes and a fixed one-byte conversion slot.
pub struct Cursor<'a> {
    source: &'a [u8],
    mode: Mode,
    parser: Parser<'a>,
    phase: Phase,
    extent: Extent,
    position: usize,
    unfolder: Unfolder,
    decoder: CharsetDecoder,
    byte: Option<u8>,
    unfolded: bool,
    noncharacter: bool,
    failure: Option<Error>,
}
impl<'a> Cursor<'a> {
    pub fn new(source: &'a [u8], mode: Mode) -> Self {
        Self {
            source,
            mode,
            parser: Parser::new(source, mode),
            phase: Phase::Validate,
            extent: Extent { start: 0, end: 0 },
            position: 0,
            unfolder: Unfolder::default(),
            decoder: CharsetDecoder::new(Charset::Utf8),
            byte: None,
            unfolded: false,
            noncharacter: false,
            failure: None,
        }
    }
    /// Final only after Complete; syntax/UTF-8 validity is established first.
    pub const fn is_encoding_problem(&self) -> bool {
        self.noncharacter
    }
    pub fn poll(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.phase, Phase::Complete) {
            return Ok(Status::Complete);
        }
        let result = self.step(now, work);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn step(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        work.charge(
            now,
            Charge {
                records: 1,
                ..Charge::default()
            },
        )?;
        match self.phase {
            Phase::Validate => {
                if self.parser.poll(now, work)? == Parsed::Complete {
                    self.parser = Parser::new(self.source, self.mode);
                    self.phase = Phase::Replay;
                }
                Ok(Status::Yield)
            }
            Phase::Replay => match self.parser.poll(now, work)? {
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
            Phase::Unfold => self.unfold(now, work),
            Phase::Decode => self.decode(now, work),
            Phase::Complete => Ok(Status::Complete),
        }
    }
    fn unfold(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        if self.byte.is_some() || self.unfolded {
            return Err(Error::InvalidState);
        }
        let input = self
            .source
            .get(self.position..self.extent.end)
            .ok_or(Error::InvalidState)?;
        let mut output = [0; 1];
        let progress = self.unfolder.poll(input, &mut output, true, now, work)?;
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
    fn decode(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        let input = self.byte.as_slice();
        let progress = self
            .decoder
            .poll(input, self.unfolded, now, work)
            .map_err(|error| match error {
                mime_charset::Error::Work(stop) => Error::Work(stop),
                mime_charset::Error::InvalidState => Error::InvalidState,
            })?;
        if progress.consumed > input.len() || self.decoder.is_encoding_problem() {
            return Err(Error::InvalidState);
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
                self.phase = Phase::Replay;
                Ok(Status::Yield)
            }
            Decoded::Scalar(value) => {
                let scalar = u32::from(value);
                let value = if matches!(scalar, 0xfdd0..=0xfdef) || scalar & 0xffff >= 0xfffe {
                    self.noncharacter = true;
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
    use crate::{admission::work::Stop, ports::Deadline};
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
