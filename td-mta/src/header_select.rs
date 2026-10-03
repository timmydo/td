//! Resident header traversal with bounded case-insensitive field matching.
use crate::{
    admission::work::{Meter, Stop},
    header_property::{Occurrence, Property},
    header_work::{Aggregate, Charge, Work},
    mime_headers::{self, End, Field, Scanner},
    nfc::HeaderBudget,
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
    InterpretationLimit,
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => f.write_str("incomplete resident headers"),
            Self::Headers(error) => write!(f, "header selection: {error}"),
            Self::Work(error) => write!(f, "header selection work: {error}"),
            Self::InterpretationLimit => f.write_str("header selection interpretation limit"),
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
impl From<crate::decode_work::Error> for Error {
    fn from(error: crate::decode_work::Error) -> Self {
        match error {
            crate::decode_work::Error::Work(stop) => Self::Work(stop),
            crate::decode_work::Error::InterpretationLimit => Self::InterpretationLimit,
            crate::decode_work::Error::InvalidState => Self::InvalidState,
        }
    }
}
impl From<mime_headers::Error> for Error {
    fn from(error: mime_headers::Error) -> Self {
        match error {
            mime_headers::Error::Work(stop) => Self::Work(stop),
            mime_headers::Error::InterpretationLimit => Self::InterpretationLimit,
            mime_headers::Error::InvalidState => Self::InvalidState,
            other => Self::Headers(other),
        }
    }
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Mode {
    Job,
    Aggregate,
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
    credit: u8,
    mode: Option<Mode>,
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
            credit: 0,
            mode: None,
        }
    }
    pub fn poll(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        self.bind_mode(Mode::Job)?;
        self.poll_with_work(now, work)
    }
    /// Use from the first poll, without interleaving plain `poll` calls.
    /// Retain the same live job meter and email budget across every projection.
    pub fn poll_with_budget(
        &mut self,
        now: Tick,
        work: &mut Meter,
        budget: &mut HeaderBudget,
    ) -> Result<Status, Error> {
        self.bind_mode(Mode::Aggregate)?;
        let mut credit = self.credit;
        let result = self.poll_with_work(now, &mut Aggregate::new(work, budget, &mut credit));
        self.credit = credit;
        result
    }
    fn bind_mode(&mut self, mode: Mode) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.mode.is_some_and(|previous| previous != mode) {
            self.failure = Some(Error::InvalidState);
            return Err(Error::InvalidState);
        }
        self.mode = Some(mode);
        Ok(())
    }
    fn poll_with_work(&mut self, now: Tick, work: &mut impl Work) -> Result<Status, Error> {
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
    fn step(&mut self, now: Tick, work: &mut impl Work) -> Result<Status, Error> {
        if let Some(end) = self.end {
            work.charge(
                now,
                Charge {
                    steps: 1,
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
        let progress =
            self.scanner
                .poll_with_work(input, self.source_end == SourceEnd::Eof, now, work)?;
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
    fn compare(&mut self, field: Field, now: Tick, work: &mut impl Work) -> Result<Status, Error> {
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
                    steps: 1,
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
                visits: (count as u64) * 2,
                steps: (count as u64).max(1),
                records: 1,
            },
        )?;
        let end = self
            .compared
            .checked_add(count)
            .ok_or(Error::InvalidState)?;
        let left = name.get(self.compared..end).ok_or(Error::InvalidState)?;
        let right = wanted.get(self.compared..end).ok_or(Error::InvalidState)?;
        let mut matches = true;
        for (left, right) in left.iter().zip(right) {
            matches &= left.eq_ignore_ascii_case(right);
        }
        if !matches {
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
    use crate::{admission::work::Charge, header_property, ports::Deadline};
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
    fn budget_with(bytes: u64, steps: u64) -> HeaderBudget {
        let mut budget = HeaderBudget::new();
        let mut setup = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: u64::MAX,
                records: u64::MAX,
                ..Charge::default()
            },
        );
        let mut credit = 0;
        budget
            .charge(
                &mut setup,
                Tick(1),
                budget.source_bytes_remaining() - bytes,
                budget.steps_remaining() - steps,
                &mut credit,
            )
            .unwrap();
        budget
    }
    #[test]
    fn aggregate_and_plain_selection_keep_their_distinct_record_accounting() {
        for (key, steps, records) in [("header:X:all", 9, 3), ("header:X", 10, 4)] {
            let mut cursor = Cursor::new(b"X:a\n\n", 0, 100, property(key), SourceEnd::Eof);
            assert!(std::mem::size_of_val(&cursor) <= 384);
            let mut work = work();
            let before = work.remaining();
            let mut budget = budget_with(8, steps);
            let mut found = 0;
            let mut complete = None;
            for _ in 0..100 {
                match cursor
                    .poll_with_budget(Tick(1), &mut work, &mut budget)
                    .unwrap()
                {
                    Status::Match(field) => {
                        assert_eq!((field.value_start, field.value_end), (2, 3));
                        found += 1;
                    }
                    Status::Yield => {}
                    Status::Complete(end) => {
                        complete = Some(end);
                        break;
                    }
                }
            }
            assert_eq!(found, 1);
            let end = complete.unwrap();
            assert_eq!(budget.source_bytes_remaining(), 0);
            assert_eq!(budget.steps_remaining(), 0);
            assert_eq!(before.io_bytes - work.remaining().io_bytes, 8);
            assert_eq!(before.records - work.remaining().records, 1);
            let after = work.remaining();
            assert_eq!(
                cursor.poll_with_budget(Tick(100), &mut work, &mut budget),
                Ok(Status::Complete(end))
            );
            assert_eq!(work.remaining(), after);
            let mut legacy = Cursor::new(b"X:a\n\n", 0, 100, property(key), SourceEnd::Eof);
            let mut legacy_work = self::work();
            for _ in 0..100 {
                if matches!(
                    legacy.poll(Tick(1), &mut legacy_work).unwrap(),
                    Status::Complete(_)
                ) {
                    break;
                }
            }
            assert!(legacy.complete);
            assert_eq!(before.io_bytes - legacy_work.remaining().io_bytes, 8);
            assert_eq!(before.records - legacy_work.remaining().records, records);
        }
    }
    #[test]
    fn aggregate_refusals_precede_scanning_comparison_and_cross_projection_work() {
        for (bytes, steps, scanned) in [
            (0, 100, false),
            (100, 0, false),
            (6, 100, true),
            (100, 6, true),
        ] {
            let mut budget = budget_with(bytes, steps);
            let mut work = work();
            let mut cursor =
                Cursor::new(b"X:a\n\n", 0, 100, property("header:X:all"), SourceEnd::Eof);
            if scanned {
                assert_eq!(
                    cursor.poll_with_budget(Tick(1), &mut work, &mut budget),
                    Ok(Status::Yield)
                );
                assert!(cursor.candidate.is_some());
            }
            let before = work.remaining();
            let remaining = (budget.source_bytes_remaining(), budget.steps_remaining());
            let consumed = cursor.consumed;
            let error = Error::InterpretationLimit;
            assert_eq!(
                cursor.poll_with_budget(Tick(1), &mut work, &mut budget),
                Err(error)
            );
            assert_eq!(cursor.consumed, consumed);
            assert_eq!(cursor.compared, 0);
            assert_eq!(work.remaining(), before);
            assert_eq!(
                (budget.source_bytes_remaining(), budget.steps_remaining()),
                remaining
            );
            assert_eq!(cursor.poll(Tick(1), &mut self::work()), Err(error));
            let mut next =
                Cursor::new(b"X:b\n\n", 0, 100, property("header:X:all"), SourceEnd::Eof);
            assert_eq!(
                next.poll_with_budget(Tick(1), &mut work, &mut budget),
                Err(error)
            );
            let mut scratch = crate::nfc::Scratch::new();
            let mut nfc = crate::nfc::Cursor::new("a", &mut scratch, &mut work, &mut budget);
            assert_eq!(
                nfc.poll(Tick(1)),
                Err(crate::nfc::Error::InterpretationLimit)
            );
            assert_eq!(work.remaining(), before);
        }
        let mut budget = budget_with(8, 9);
        let mut work = work();
        for attempt in 0..2 {
            let mut cursor =
                Cursor::new(b"X:a\n\n", 0, 100, property("header:X:all"), SourceEnd::Eof);
            let mut ended = false;
            for _ in 0..100 {
                match cursor.poll_with_budget(Tick(1), &mut work, &mut budget) {
                    Ok(Status::Complete(_)) => {
                        assert_eq!(attempt, 0);
                        ended = true;
                        break;
                    }
                    Err(Error::InterpretationLimit) => {
                        assert_eq!(attempt, 1);
                        ended = true;
                        break;
                    }
                    Ok(_) => {}
                    Err(error) => panic!("unexpected {error}"),
                }
            }
            assert!(ended);
        }
    }
    #[test]
    fn aggregate_long_values_remain_bounded_and_job_refusals_are_distinct() {
        let mut source = b"X:".to_vec();
        source.extend(std::iter::repeat_n(b'a', 1024 * 1024));
        source.extend_from_slice(b"\n\n");
        let mut budget = HeaderBudget::new();
        let mut work = work();
        let before = work.remaining();
        let initial = (budget.source_bytes_remaining(), budget.steps_remaining());
        let mut cursor = Cursor::new(
            &source,
            0,
            source.len() as u64,
            property("header:X:all"),
            SourceEnd::Eof,
        );
        let mut completed = false;
        for _ in 0..10_000 {
            let b = work.remaining();
            let steps = budget.steps_remaining();
            let status = cursor
                .poll_with_budget(Tick(1), &mut work, &mut budget)
                .unwrap();
            assert!(b.io_bytes - work.remaining().io_bytes <= 256);
            assert!(b.records - work.remaining().records <= 16);
            assert!(steps - budget.steps_remaining() <= 256);
            if matches!(status, Status::Complete(_)) {
                completed = true;
                break;
            }
        }
        assert!(completed);
        let visits = source.len() as u64 + 3;
        let steps = source.len() as u64 + 4;
        assert_eq!(initial.0 - budget.source_bytes_remaining(), visits);
        assert_eq!(initial.1 - budget.steps_remaining(), steps);
        assert_eq!(before.io_bytes - work.remaining().io_bytes, visits);
        assert_eq!(
            before.records - work.remaining().records,
            steps.div_ceil(16)
        );
        for (io_bytes, records, now, stop) in [
            (0, 100, Tick(1), Stop::IoBytes),
            (100, 0, Tick(1), Stop::Records),
            (100, 100, Tick(100), Stop::Deadline),
        ] {
            let mut limited = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes,
                    records,
                    ..Charge::default()
                },
            );
            let mut budget = HeaderBudget::new();
            let initial = (budget.source_bytes_remaining(), budget.steps_remaining());
            let mut cursor = Cursor::new(b"X:a", 0, 100, property("header:X:all"), SourceEnd::Eof);
            assert_eq!(
                cursor.poll_with_budget(now, &mut limited, &mut budget),
                Err(Error::Work(stop))
            );
            assert_eq!(cursor.consumed, 0);
            assert_eq!(
                (budget.source_bytes_remaining(), budget.steps_remaining()),
                initial
            );
            assert_eq!(
                cursor.poll_with_budget(Tick(1), &mut self::work(), &mut HeaderBudget::new()),
                Err(Error::Work(stop))
            );
        }
    }
    #[test]
    fn comparison_chunks_charge_every_pair_and_reset_between_candidates() {
        let name = "x".repeat(40);
        let key = format!("header:{name}:all");
        let source = format!(
            "{}{}: a\n{}{}: b\nshort: c\n{name}: d\n\n",
            "x".repeat(32),
            "y".repeat(8),
            "y".repeat(32),
            "x".repeat(8)
        );
        let mut cursor = Cursor::new(source.as_bytes(), 0, 1000, property(&key), SourceEnd::Eof);
        let mut budget = HeaderBudget::new();
        let mut work = work();
        let mut comparisons = Vec::new();
        let mut matches = 0;
        let mut complete = false;
        for _ in 0..100 {
            let candidate = cursor.candidate;
            let compared = cursor.compared;
            let before = (budget.source_bytes_remaining(), budget.steps_remaining());
            let status = cursor
                .poll_with_budget(Tick(1), &mut work, &mut budget)
                .unwrap();
            if candidate.is_some() {
                comparisons.push((
                    compared,
                    before.0 - budget.source_bytes_remaining(),
                    before.1 - budget.steps_remaining(),
                ));
            } else if cursor.candidate.is_some() {
                assert_eq!(cursor.compared, 0);
            }
            match status {
                Status::Match(field) => {
                    matches += 1;
                    assert_eq!(
                        &source.as_bytes()[field.value_start as usize..field.value_end as usize],
                        b" d"
                    );
                }
                Status::Complete(_) => {
                    complete = true;
                    break;
                }
                Status::Yield => {}
            }
        }
        assert!(complete);
        assert_eq!(matches, 1);
        assert_eq!(
            comparisons,
            [
                (0, 64, 32),
                (32, 16, 8),
                (0, 64, 32),
                (0, 0, 1),
                (0, 64, 32),
                (32, 16, 8)
            ]
        );
    }
    #[test]
    fn switching_admission_modes_retires_without_debiting_either_budget() {
        for aggregate_first in [false, true] {
            let mut cursor =
                Cursor::new(b"X:a\n\n", 0, 100, property("header:X:all"), SourceEnd::Eof);
            let mut budget = HeaderBudget::new();
            let mut work = work();
            if aggregate_first {
                cursor
                    .poll_with_budget(Tick(1), &mut work, &mut budget)
                    .unwrap();
            } else {
                cursor.poll(Tick(1), &mut work).unwrap();
            }
            let before = (
                work.remaining(),
                budget.steps_remaining(),
                budget.source_bytes_remaining(),
            );
            let result = if aggregate_first {
                cursor.poll(Tick(1), &mut work)
            } else {
                cursor.poll_with_budget(Tick(1), &mut work, &mut budget)
            };
            assert_eq!(result, Err(Error::InvalidState));
            assert_eq!(cursor.poll(Tick(1), &mut work), Err(Error::InvalidState));
            assert_eq!(
                cursor.poll_with_budget(Tick(1), &mut work, &mut budget),
                Err(Error::InvalidState)
            );
            assert_eq!(
                (
                    work.remaining(),
                    budget.steps_remaining(),
                    budget.source_bytes_remaining()
                ),
                before
            );
        }
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
