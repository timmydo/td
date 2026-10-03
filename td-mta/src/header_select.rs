//! Resident header traversal with bounded case-insensitive field matching.
use crate::{
    admission::work::{Charge, Meter, Stop},
    header_property::{Occurrence, Property},
    mime_headers::{self, End, Field, Scanner},
    ports::Tick,
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceEnd {
    Eof,
    Prefix,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    /// Provisional until Complete; discard all matches on any later failure.
    Match(Field),
    Yield,
    Complete(End),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Truncated,
    Headers(mime_headers::Error),
    Work(Stop),
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => f.write_str("incomplete resident headers"),
            Self::Headers(error) => write!(f, "header selection: {error}"),
            Self::Work(error) => write!(f, "header selection work: {error}"),
            Self::InvalidState => f.write_str("invalid header selection state"),
        }
    }
}
impl std::error::Error for Error {}
impl From<Stop> for Error {
    fn from(error: Stop) -> Self {
        Self::Work(error)
    }
}
impl From<mime_headers::Error> for Error {
    fn from(error: mime_headers::Error) -> Self {
        match error {
            mime_headers::Error::Work(stop) => Self::Work(stop),
            other => Self::Headers(other),
        }
    }
}
/// Source must include the complete header section and its separator, or actual EOF.
/// It may include body lookahead; the scanner stops at the header/body boundary.
pub struct Cursor<'a> {
    source: &'a [u8],
    source_end: SourceEnd,
    property: Property<'a>,
    base: u64,
    consumed: usize,
    scanner: Scanner,
    candidate: Option<Field>,
    compared: usize,
    last: Option<Field>,
    end: Option<End>,
    complete: bool,
    failure: Option<Error>,
}
impl<'a> Cursor<'a> {
    pub const fn new(
        source: &'a [u8],
        base: u64,
        header_limit: u64,
        property: Property<'a>,
        source_end: SourceEnd,
    ) -> Self {
        Self {
            source,
            source_end,
            property,
            base,
            consumed: 0,
            scanner: Scanner::new(base, header_limit),
            candidate: None,
            compared: 0,
            last: None,
            end: None,
            complete: false,
            failure: None,
        }
    }
    pub fn poll(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.complete {
            return self.end.map(Status::Complete).ok_or(Error::InvalidState);
        }
        let result = self.step(now, work);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn step(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        if let Some(end) = self.end {
            work.charge(
                now,
                Charge {
                    records: 1,
                    ..Charge::default()
                },
            )?;
            if let Some(field) = self.last.take() {
                return Ok(Status::Match(field));
            }
            self.complete = true;
            return Ok(Status::Complete(end));
        }
        if let Some(field) = self.candidate {
            return self.compare(field, now, work);
        }
        let input = self
            .source
            .get(self.consumed..)
            .ok_or(Error::InvalidState)?;
        let progress = self
            .scanner
            .poll(input, self.source_end == SourceEnd::Eof, now, work)?;
        self.consumed = self
            .consumed
            .checked_add(progress.consumed)
            .ok_or(Error::InvalidState)?;
        match progress.status {
            mime_headers::Status::Field(field) => {
                self.candidate = Some(field);
                self.compared = 0;
            }
            mime_headers::Status::Complete(end) => {
                self.end = Some(end);
            }
            mime_headers::Status::Yield => {}
            mime_headers::Status::NeedInput => return Err(Error::Truncated),
        }
        Ok(Status::Yield)
    }
    fn compare(&mut self, field: Field, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        let start = usize::try_from(
            field
                .name_start
                .checked_sub(self.base)
                .ok_or(Error::InvalidState)?,
        )
        .map_err(|_| Error::InvalidState)?;
        let end = usize::try_from(
            field
                .name_end
                .checked_sub(self.base)
                .ok_or(Error::InvalidState)?,
        )
        .map_err(|_| Error::InvalidState)?;
        let name = self.source.get(start..end).ok_or(Error::InvalidState)?;
        let wanted = self.property.name().as_bytes();
        if name.len() != wanted.len() {
            work.charge(
                now,
                Charge {
                    records: 1,
                    ..Charge::default()
                },
            )?;
            self.candidate = None;
            return Ok(Status::Yield);
        }
        let count = name
            .len()
            .checked_sub(self.compared)
            .ok_or(Error::InvalidState)?
            .min(32);
        work.charge(
            now,
            Charge {
                io_bytes: (count as u64) * 2,
                records: 1,
                ..Charge::default()
            },
        )?;
        let end = self
            .compared
            .checked_add(count)
            .ok_or(Error::InvalidState)?;
        let left = name.get(self.compared..end).ok_or(Error::InvalidState)?;
        let right = wanted.get(self.compared..end).ok_or(Error::InvalidState)?;
        if !left.eq_ignore_ascii_case(right) {
            self.candidate = None;
            return Ok(Status::Yield);
        }
        self.compared = end;
        if end != name.len() {
            return Ok(Status::Yield);
        }
        self.candidate = None;
        if self.property.occurrence() == Occurrence::All {
            return Ok(Status::Match(field));
        }
        self.last = Some(field);
        Ok(Status::Yield)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{header_property, ports::Deadline};
    fn work() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 10_000_000,
                records: 2_000_000,
                ..Charge::default()
            },
        )
    }
    fn property(key: &str) -> Property<'_> {
        let mut cursor = header_property::Cursor::new(key, header_property::Context::Email);
        let mut work = work();
        for _ in 0..100_000 {
            if let header_property::Status::Complete(value) =
                cursor.poll(Tick(1), &mut work).unwrap()
            {
                return value.unwrap();
            }
        }
        panic!("property did not complete");
    }
    fn select(source: &[u8], base: u64, key: &str) -> (Vec<Field>, End) {
        let mut cursor = Cursor::new(
            source,
            base,
            source.len() as u64,
            property(key),
            SourceEnd::Eof,
        );
        assert!(std::mem::size_of_val(&cursor) <= 384);
        let mut work = work();
        let mut fields = Vec::new();
        for _ in 0..100_000 {
            let before = work.remaining();
            let status = cursor.poll(Tick(1), &mut work).unwrap();
            let after = work.remaining();
            assert!(before.io_bytes - after.io_bytes <= 256);
            assert!(before.records - after.records <= 1);
            assert_eq!(before.output_bytes, after.output_bytes);
            match status {
                Status::Match(field) => fields.push(field),
                Status::Yield => {}
                Status::Complete(end) => {
                    assert_eq!(cursor.poll(Tick(100), &mut work), Ok(Status::Complete(end)));
                    assert_eq!(work.remaining(), after);
                    return (fields, end);
                }
            }
        }
        panic!("selection did not finish");
    }
    #[test]
    fn occurrences_preserve_wire_order_offsets_and_raw_values() {
        let source = b"Subject: first\r\nX-Ignored: v\r\nsUbJeCt \t: second\r\n\tfold\r\nSubject:\r\n\r\nbody";
        for base in [0, 4096] {
            let (all, end) = select(source, base, "header:SUBject:asText:all");
            assert_eq!(all.len(), 3);
            for (field, (name, value)) in all.iter().zip([
                (b"Subject".as_slice(), b" first".as_slice()),
                (b"sUbJeCt", b" second\r\n\tfold"),
                (b"Subject", b""),
            ]) {
                assert_eq!(
                    &source[(field.name_start - base) as usize..(field.name_end - base) as usize],
                    name
                );
                assert_eq!(
                    &source[(field.value_start - base) as usize..(field.value_end - base) as usize],
                    value
                );
            }
            assert_eq!(end.body_start, base + source.len() as u64 - 4);
            let (last, last_end) = select(source, base, "subject");
            assert_eq!(last, all[2..]);
            assert_eq!(last_end, end);
            assert!(select(source, base, "header:Missing:all").0.is_empty());
            assert!(select(source, base, "header:Missing").0.is_empty());
        }
    }
    #[test]
    fn eof_and_malformed_body_boundaries_do_not_invent_fields() {
        for (source, body, count) in [
            (b"".as_slice(), 0, 0),
            (b"Subject: a", 10, 1),
            (b"Subject: a\nSubject: b", 21, 2),
            (b"Subject: a\nnot a field\nSubject: body", 11, 1),
            (b" orphan\nSubject: body", 0, 0),
            (b"Subject: a\r\n\r\nSubject: body", 14, 1),
        ] {
            for base in [0, 4096] {
                let (fields, end) = select(source, base, "header:Subject:all");
                assert_eq!(fields.len(), count, "{source:?}");
                assert_eq!(end.body_start, base + body, "{source:?}");
                let (last, last_end) = select(source, base, "subject");
                assert_eq!(
                    last.as_slice(),
                    fields.last().map(std::slice::from_ref).unwrap_or(&[])
                );
                assert_eq!(last_end, end);
                for field in fields {
                    assert_eq!(
                        &source
                            [(field.name_start - base) as usize..(field.name_end - base) as usize],
                        b"Subject"
                    );
                    assert!(field.value_start <= field.value_end);
                    assert!(field.value_end <= base + source.len() as u64);
                }
            }
        }
    }
    #[test]
    fn partial_resident_input_cannot_claim_eof() {
        let source = b"Subject: abcdefghij\r\n\r\n";
        for len in 0..source.len() {
            let mut cursor = Cursor::new(
                &source[..len],
                0,
                100,
                property("subject"),
                SourceEnd::Prefix,
            );
            let mut work = work();
            let mut refused = false;
            for _ in 0..100 {
                match cursor.poll(Tick(1), &mut work) {
                    Ok(Status::Yield) => {}
                    Err(error) => {
                        assert_eq!(error, Error::Truncated, "prefix {len}");
                        assert_eq!(cursor.poll(Tick(1), &mut self::work()), Err(error));
                        refused = true;
                        break;
                    }
                    Ok(_) => panic!("truncated prefix succeeded"),
                }
            }
            assert!(refused);
        }
        for (source, end) in [
            (source.as_slice(), SourceEnd::Prefix),
            (b"Subject: x", SourceEnd::Eof),
        ] {
            let mut cursor = Cursor::new(source, 0, 100, property("subject"), end);
            let mut work = work();
            let mut found = 0;
            let mut complete = false;
            for _ in 0..100 {
                match cursor.poll(Tick(1), &mut work).unwrap() {
                    Status::Yield => {}
                    Status::Match(_) => found += 1,
                    Status::Complete(_) => {
                        complete = true;
                        break;
                    }
                }
            }
            assert!(complete);
            assert_eq!(found, 1);
        }
    }
    #[test]
    fn long_name_comparisons_yield_and_charge_both_operands() {
        let name = "x".repeat(10_000);
        let key = format!("header:{name}:all");
        let source = format!(
            "{}: one\n{}y: other\n{}: two\n\n",
            name.to_uppercase(),
            &name[..9999],
            name
        );
        let (fields, _) = select(source.as_bytes(), 0, &key);
        assert_eq!(fields.len(), 2);
        let mut cursor = Cursor::new(
            source.as_bytes(),
            0,
            source.len() as u64,
            property(&key),
            SourceEnd::Eof,
        );
        let mut work = work();
        while cursor.candidate.is_none() {
            assert_eq!(cursor.poll(Tick(1), &mut work), Ok(Status::Yield));
        }
        let before = work.remaining();
        assert_eq!(cursor.poll(Tick(1), &mut work), Ok(Status::Yield));
        assert_eq!(before.io_bytes - work.remaining().io_bytes, 64);
        assert_eq!(cursor.compared, 32);
        assert_eq!(
            cursor.poll(Tick(100), &mut work),
            Err(Error::Work(Stop::Deadline))
        );
        assert_eq!(
            cursor.poll(Tick(1), &mut self::work()),
            Err(Error::Work(Stop::Deadline))
        );
    }
    #[test]
    fn provisional_matches_and_last_selection_fail_closed() {
        let source = b"Subject: first\nOther: too long\n\n";
        for (key, count) in [("header:Subject:all", 1), ("subject", 0)] {
            let mut cursor = Cursor::new(source, 0, 16, property(key), SourceEnd::Eof);
            let mut work = work();
            let mut matches = 0;
            let mut refused = false;
            for _ in 0..100 {
                match cursor.poll(Tick(1), &mut work) {
                    Ok(Status::Match(_)) => matches += 1,
                    Ok(Status::Yield) => {}
                    Ok(Status::Complete(_)) => panic!("accepted over-limit headers"),
                    Err(error) => {
                        assert_eq!(error, Error::Headers(mime_headers::Error::HeaderLimit));
                        assert_eq!(cursor.poll(Tick(1), &mut self::work()), Err(error));
                        refused = true;
                        break;
                    }
                }
            }
            assert!(refused);
            assert_eq!(matches, count);
        }
        for (capacity, expected) in [
            (
                Charge {
                    io_bytes: 13,
                    records: 100,
                    ..Charge::default()
                },
                Stop::IoBytes,
            ),
            (
                Charge {
                    io_bytes: 100,
                    records: 0,
                    ..Charge::default()
                },
                Stop::Records,
            ),
        ] {
            let mut cursor =
                Cursor::new(b"Subject: x", 0, 100, property("subject"), SourceEnd::Eof);
            let mut scan_work = work();
            while cursor.candidate.is_none() {
                assert_eq!(cursor.poll(Tick(1), &mut scan_work), Ok(Status::Yield));
            }
            let mut limited = Meter::new(Deadline::after(Tick(0), 100).unwrap(), capacity);
            assert_eq!(
                cursor.poll(Tick(1), &mut limited),
                Err(Error::Work(expected))
            );
            assert_eq!(limited.remaining(), capacity);
            assert_eq!(
                cursor.poll(Tick(1), &mut work()),
                Err(Error::Work(expected))
            );
        }
        let mut pending = Cursor::new(b"Subject: x", 0, 100, property("subject"), SourceEnd::Eof);
        let mut pending_work = work();
        while pending.end.is_none() {
            assert_eq!(pending.poll(Tick(1), &mut pending_work), Ok(Status::Yield));
        }
        assert!(matches!(
            pending.poll(Tick(1), &mut pending_work),
            Ok(Status::Match(_))
        ));
        assert_eq!(
            pending.poll(Tick(100), &mut pending_work),
            Err(Error::Work(Stop::Deadline))
        );
        assert_eq!(
            pending.poll(Tick(1), &mut work()),
            Err(Error::Work(Stop::Deadline))
        );
        let selected = property("subject");
        let mut cursor = Cursor::new(b"Subject: x", u64::MAX - 2, 100, selected, SourceEnd::Eof);
        assert_eq!(
            cursor.poll(Tick(1), &mut work()),
            Err(Error::Headers(mime_headers::Error::Offset))
        );
        assert_eq!(
            cursor.poll(Tick(1), &mut work()),
            Err(Error::Headers(mime_headers::Error::Offset))
        );
        let mut cursor = Cursor::new(b"Subject: x", 0, 100, selected, SourceEnd::Eof);
        let mut limited = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                records: 100,
                ..Charge::default()
            },
        );
        assert_eq!(
            cursor.poll(Tick(1), &mut limited),
            Err(Error::Work(Stop::IoBytes))
        );
        assert_eq!(
            cursor.poll(Tick(1), &mut work()),
            Err(Error::Work(Stop::IoBytes))
        );
    }
}
