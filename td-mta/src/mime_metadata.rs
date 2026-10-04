//! Resident first-valid MIME metadata selection over authorized headers.
use crate::{
    admission::work::{Meter, Stop},
    decode_work,
    header_select::SourceEnd,
    header_work::{self, Aggregate},
    mime_fields::{self, Head, Kind},
    mime_headers::{self, End, Field, Scanner},
    nfc::HeaderBudget,
    ports::Tick,
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DefaultType {
    TextPlain,
    MessageRfc822,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Context {
    Normal,
    DigestChild,
}
/// Head offsets are relative to the exact field value, field offsets absolute.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Selected {
    pub field: Field,
    pub head: Head,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentType {
    Default(DefaultType),
    Field(Selected),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Selection {
    pub content_type: ContentType,
    pub content_disposition: Option<Selected>,
    pub transfer_encoding: Option<Selected>,
    pub end: End,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Complete,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidRange,
    Truncated,
    Headers(mime_headers::Error),
    NestingLimit,
    Work(Stop),
    InterpretationLimit,
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRange => f.write_str("invalid MIME header range"),
            Self::Truncated => f.write_str("incomplete resident MIME headers"),
            Self::Headers(error) => write!(f, "MIME headers: {error}"),
            Self::NestingLimit => f.write_str("MIME metadata nesting limit"),
            Self::Work(error) => write!(f, "MIME metadata work: {error}"),
            Self::InterpretationLimit => f.write_str("MIME metadata interpretation limit"),
            Self::InvalidState => f.write_str("invalid MIME metadata state"),
        }
    }
}
impl std::error::Error for Error {}
impl From<decode_work::Error> for Error {
    fn from(error: decode_work::Error) -> Self {
        match error {
            decode_work::Error::Work(error) => Self::Work(error),
            decode_work::Error::InterpretationLimit => Self::InterpretationLimit,
            decode_work::Error::InvalidState => Self::InvalidState,
        }
    }
}
impl From<mime_headers::Error> for Error {
    fn from(error: mime_headers::Error) -> Self {
        match error {
            mime_headers::Error::Work(error) => Self::Work(error),
            mime_headers::Error::InterpretationLimit => Self::InterpretationLimit,
            mime_headers::Error::InvalidState => Self::InvalidState,
            error => Self::Headers(error),
        }
    }
}
trait Work: header_work::Work + decode_work::Work {}
impl<W: header_work::Work + decode_work::Work> Work for W {}
#[derive(Clone, Copy)]
enum Phase {
    Scan,
    Match,
    Parse,
    Finish,
    Complete,
}
fn slot(kind: Kind) -> usize {
    match kind {
        Kind::ContentType => 0,
        Kind::ContentDisposition => 1,
        Kind::TransferEncoding => 2,
    }
}
/// Source includes a definitive scanner boundary or actual EOF.
/// End.body_start is authoritative even after tentative body lookahead.
/// This borrowed view grants no raw-blob or part-location authority.
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {}
/// cloned::<td_mta::mime_metadata::Cursor<'_>>();
/// ```
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {}
/// copied::<td_mta::mime_metadata::Cursor<'_>>();
/// ```
pub struct Cursor<'a> {
    source: &'a [u8],
    base: u64,
    source_end: SourceEnd,
    context: Context,
    scanner: Scanner,
    consumed: usize,
    candidate: Option<Field>,
    kind: Option<Kind>,
    syntax: Option<mime_fields::Cursor<'a>>,
    head: Option<Head>,
    selected: [Option<Selected>; 3],
    end: Option<End>,
    phase: Phase,
    failure: Option<Error>,
}
impl<'a> Cursor<'a> {
    pub fn new(
        source: &'a [u8],
        base: u64,
        header_limit: u64,
        context: Context,
        source_end: SourceEnd,
    ) -> Result<Self, Error> {
        base.checked_add(u64::try_from(source.len()).map_err(|_| Error::InvalidRange)?)
            .ok_or(Error::InvalidRange)?;
        Ok(Self {
            source,
            base,
            source_end,
            context,
            scanner: Scanner::new(base, header_limit),
            consumed: 0,
            candidate: None,
            kind: None,
            syntax: None,
            head: None,
            selected: [None; 3],
            end: None,
            phase: Phase::Scan,
            failure: None,
        })
    }
    pub fn poll(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        self.poll_with_work(now, work)
    }
    fn poll_with_work(&mut self, now: Tick, work: &mut impl Work) -> Result<Status, Error> {
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
    /// Cached metadata is available only after the complete section validates.
    /// The caller checks fresh admission before later publication.
    pub fn selection(&self) -> Result<Option<Selection>, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.phase, Phase::Complete) {
            self.result().map(Some)
        } else {
            Ok(None)
        }
    }
    fn range(&self, start: u64, end: u64) -> Result<&'a [u8], Error> {
        let start = usize::try_from(start.checked_sub(self.base).ok_or(Error::InvalidState)?)
            .map_err(|_| Error::InvalidState)?;
        let end = usize::try_from(end.checked_sub(self.base).ok_or(Error::InvalidState)?)
            .map_err(|_| Error::InvalidState)?;
        self.source.get(start..end).ok_or(Error::InvalidState)
    }
    fn result(&self) -> Result<Selection, Error> {
        let default = match self.context {
            Context::Normal => DefaultType::TextPlain,
            Context::DigestChild => DefaultType::MessageRfc822,
        };
        Ok(Selection {
            content_type: self
                .selected
                .first()
                .copied()
                .flatten()
                .map(ContentType::Field)
                .unwrap_or(ContentType::Default(default)),
            content_disposition: self.selected.get(1).copied().flatten(),
            transfer_encoding: self.selected.get(2).copied().flatten(),
            end: self.end.ok_or(Error::InvalidState)?,
        })
    }
    fn reset(&mut self) {
        self.candidate = None;
        self.syntax = None;
        self.head = None;
        self.kind = None;
        self.phase = Phase::Scan;
    }
    fn step(&mut self, now: Tick, work: &mut impl Work) -> Result<Status, Error> {
        match self.phase {
            Phase::Scan => {
                let input = self
                    .source
                    .get(self.consumed..)
                    .ok_or(Error::InvalidState)?;
                let progress = self.scanner.poll_with_work(
                    input,
                    self.source_end == SourceEnd::Eof,
                    now,
                    work,
                )?;
                self.consumed = self
                    .consumed
                    .checked_add(progress.consumed)
                    .ok_or(Error::InvalidState)?;
                match progress.status {
                    mime_headers::Status::Field(field) => {
                        self.candidate = Some(field);
                        self.phase = Phase::Match;
                    }
                    mime_headers::Status::Complete(end) => {
                        self.end = Some(end);
                        self.phase = Phase::Finish;
                    }
                    mime_headers::Status::Yield => {}
                    mime_headers::Status::NeedInput => return Err(Error::Truncated),
                }
            }
            Phase::Match => {
                let field = self.candidate.ok_or(Error::InvalidState)?;
                let name = self.range(field.name_start, field.name_end)?;
                let (wanted, kind): (&[u8], Kind) = match name.len() {
                    12 => (b"Content-Type", Kind::ContentType),
                    19 => (b"Content-Disposition", Kind::ContentDisposition),
                    25 => (b"Content-Transfer-Encoding", Kind::TransferEncoding),
                    _ => {
                        header_work::Work::charge(
                            work,
                            now,
                            header_work::Charge {
                                steps: 1,
                                records: 1,
                                ..header_work::Charge::default()
                            },
                        )?;
                        self.reset();
                        return Ok(Status::Yield);
                    }
                };
                header_work::Work::charge(
                    work,
                    now,
                    header_work::Charge {
                        visits: (wanted.len() as u64) * 2,
                        steps: (wanted.len() as u64).max(1),
                        records: 1,
                    },
                )?;
                if !name.eq_ignore_ascii_case(wanted) {
                    self.reset();
                    return Ok(Status::Yield);
                }
                if self
                    .selected
                    .get(slot(kind))
                    .ok_or(Error::InvalidState)?
                    .is_some()
                {
                    self.reset();
                } else {
                    self.kind = Some(kind);
                    self.syntax = Some(mime_fields::Cursor::new(
                        self.range(field.value_start, field.value_end)?,
                        kind,
                    ));
                    self.head = None;
                    self.phase = Phase::Parse;
                }
            }
            Phase::Parse => {
                let status = self
                    .syntax
                    .as_mut()
                    .ok_or(Error::InvalidState)?
                    .poll_with_work(now, work);
                match status {
                    Ok(mime_fields::Status::Head(head)) => {
                        if self.head.replace(head).is_some() {
                            return Err(Error::InvalidState);
                        }
                    }
                    Ok(mime_fields::Status::Yield | mime_fields::Status::Parameter(_)) => {}
                    Ok(mime_fields::Status::Complete) => {
                        let kind = self.kind.ok_or(Error::InvalidState)?;
                        let selected = Selected {
                            field: self.candidate.ok_or(Error::InvalidState)?,
                            head: self.head.ok_or(Error::InvalidState)?,
                        };
                        let saved = self
                            .selected
                            .get_mut(slot(kind))
                            .ok_or(Error::InvalidState)?;
                        if saved.replace(selected).is_some() {
                            return Err(Error::InvalidState);
                        }
                        self.reset();
                    }
                    Err(mime_fields::Error::Malformed) => self.reset(),
                    Err(mime_fields::Error::NestingLimit) => return Err(Error::NestingLimit),
                    Err(mime_fields::Error::Work(error)) => return Err(Error::Work(error)),
                    Err(mime_fields::Error::InterpretationLimit) => {
                        return Err(Error::InterpretationLimit)
                    }
                    Err(mime_fields::Error::InvalidState) => return Err(Error::InvalidState),
                }
            }
            Phase::Finish => {
                header_work::Work::charge(
                    work,
                    now,
                    header_work::Charge {
                        steps: 1,
                        records: 1,
                        ..header_work::Charge::default()
                    },
                )?;
                self.result()?;
                self.phase = Phase::Complete;
                return Ok(Status::Complete);
            }
            Phase::Complete => return Ok(Status::Complete),
        }
        Ok(Status::Yield)
    }
}
/// Retains the same original job/email admission across the full selection.
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {}
/// cloned::<td_mta::mime_metadata::Budgeted<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {}
/// copied::<td_mta::mime_metadata::Budgeted<'_, '_>>();
/// ```
pub struct Budgeted<'a, 'w> {
    cursor: Cursor<'a>,
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
    credit: u8,
}
impl<'a, 'w> Budgeted<'a, 'w> {
    pub fn new(
        source: &'a [u8],
        base: u64,
        header_limit: u64,
        context: Context,
        source_end: SourceEnd,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Result<Self, Error> {
        Ok(Self {
            cursor: Cursor::new(source, base, header_limit, context, source_end)?,
            work,
            budget,
            credit: 0,
        })
    }
    pub fn selection(&self) -> Result<Option<Selection>, Error> {
        self.cursor.selection()
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.cursor.failure {
            return Err(error);
        }
        let result = self
            .budget
            .charge(self.work, now, 0, 0, &mut self.credit)
            .map_err(decode_work::Error::from)
            .map_err(Error::from);
        if let Err(error) = result {
            self.cursor.failure = Some(error);
        }
        result
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        if let Some(error) = self.cursor.failure {
            return Err(error);
        }
        if matches!(self.cursor.phase, Phase::Complete) {
            return Ok(Status::Complete);
        }
        self.check_deadline(now)?;
        self.cursor.poll_with_work(
            now,
            &mut Aggregate::new(self.work, self.budget, &mut self.credit),
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{admission::work::Charge as JobCharge, ports::Deadline};
    fn work() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            JobCharge {
                io_bytes: 100_000_000,
                records: 10_000_000,
                ..JobCharge::default()
            },
        )
    }
    fn selection(source: &[u8], context: Context) -> Result<Selection, Error> {
        let mut cursor = Cursor::new(source, 100, source.len() as u64, context, SourceEnd::Eof)?;
        assert!(std::mem::size_of_val(&cursor) <= 1024);
        let mut work = work();
        for _ in 0..100_000 {
            let before = work.remaining();
            let result = cursor.poll(Tick(1), &mut work)?;
            assert!(before.io_bytes - work.remaining().io_bytes <= 256);
            assert!(before.records - work.remaining().records <= 32);
            match result {
                Status::Yield => {}
                Status::Complete => {
                    return cursor
                        .selection()
                        .and_then(|value| value.ok_or(Error::InvalidState))
                }
            }
        }
        panic!("not complete")
    }
    #[test]
    fn defaults_and_first_valid_fields() {
        assert_eq!(
            selection(b"\r\n", Context::Normal).unwrap().content_type,
            ContentType::Default(DefaultType::TextPlain)
        );
        assert_eq!(
            selection(b"\r\n", Context::DigestChild)
                .unwrap()
                .content_type,
            ContentType::Default(DefaultType::MessageRfc822)
        );
        assert_eq!(
            selection(b"Content-Type: text/html;\r\n\r\n", Context::DigestChild)
                .unwrap()
                .content_type,
            ContentType::Default(DefaultType::MessageRfc822)
        );
        let source = concat!(
            "Content-Type: multipart/mixed; boundary=a;\r\n",
            "Content-Type: TeXt/HTML\r\n",
            "Content-Type: text/plain\r\n",
            "Content-Disposition: junk;\r\n",
            "Content-Disposition: attachment; filename=x\r\n",
            "Content-Transfer-Encoding: baSE64\r\n",
            "\r\n",
            "body"
        )
        .as_bytes();
        let result = selection(source, Context::Normal).unwrap();
        let ContentType::Field(selected) = result.content_type else {
            panic!("default")
        };
        let start = (selected.field.value_start - 100) as usize;
        let head = selected.head.first;
        assert_eq!(&source[start + head.start..start + head.end], b"TeXt");
        let second = selected.head.second.unwrap();
        assert_eq!(&source[start + second.start..start + second.end], b"HTML");
        let disposition = result.content_disposition.unwrap();
        let start = (disposition.field.value_start - 100) as usize;
        assert_eq!(
            &source[start + disposition.head.first.start..start + disposition.head.first.end],
            b"attachment"
        );
        assert_eq!(
            &source[start..(disposition.field.value_end - 100) as usize],
            b" attachment; filename=x"
        );
        let transfer = result.transfer_encoding.unwrap();
        let start = (transfer.field.value_start - 100) as usize;
        assert_eq!(
            &source[start + transfer.head.first.start..start + transfer.head.first.end],
            b"baSE64"
        );
        assert_eq!(&source[(result.end.body_start - 100) as usize..], b"body");
    }
    #[test]
    fn bare_lf_mime_fields_and_folds_compose() {
        let source = concat!(
            "Content-Type: text/plain;\n charset=x\n",
            "Content-Disposition: attachment;\n filename=\"a\n b\"\n",
            "Content-Transfer-Encoding: base64\n\nbody"
        )
        .as_bytes();
        let result = selection(source, Context::Normal).unwrap();
        let ContentType::Field(selected) = result.content_type else {
            panic!("default")
        };
        let start = (selected.field.value_start - 100) as usize;
        assert_eq!(
            &source[start..(selected.field.value_end - 100) as usize],
            b" text/plain;\n charset=x"
        );
        assert!(result.content_disposition.is_some());
        assert!(result.transfer_encoding.is_some());
        assert_eq!(&source[(result.end.body_start - 100) as usize..], b"body");
    }
    #[test]
    fn late_resource_failure_is_sticky() {
        let source = format!(
            concat!(
                "Content-Type: text/plain\r\n",
                "Content-Disposition: attachment {}x{}\r\n",
                "Content-Disposition: inline\r\n\r\n"
            ),
            "(".repeat(33),
            ")".repeat(33)
        );
        let mut cursor =
            Cursor::new(source.as_bytes(), 0, 10000, Context::Normal, SourceEnd::Eof).unwrap();
        let mut admission = work();
        for _ in 0..1000 {
            match cursor.poll(Tick(1), &mut admission) {
                Ok(Status::Yield) => {}
                Ok(Status::Complete) => panic!("accepted"),
                Err(error) => {
                    assert_eq!(error, Error::NestingLimit);
                    assert_eq!(cursor.poll(Tick(1), &mut work()), Err(error));
                    return;
                }
            }
        }
        panic!("not refused")
    }
    #[test]
    fn aggregate_composition_and_final_deadline() {
        let source = b"Content-Type: text/plain; charset=utf-8\r\n\r\n";
        let mut admission = work();
        let mut budget = HeaderBudget::new();
        let mut cursor = Budgeted::new(
            source,
            0,
            1000,
            Context::Normal,
            SourceEnd::Eof,
            &mut admission,
            &mut budget,
        )
        .unwrap();
        for _ in 0..1000 {
            if matches!(cursor.cursor.phase, Phase::Finish) {
                assert_eq!(cursor.poll(Tick(100)), Err(Error::Work(Stop::Deadline)));
                assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
                return;
            }
            assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
        }
        panic!("not finish")
    }
    #[test]
    fn invalid_suffixes_default_and_later_duplicates_are_ignored() {
        for source in [
            b"Content-Type: text/html;\r\n\r\n".as_slice(),
            b"Content-Type: text/plain; name=\"caf\xe9\"\r\n\r\n",
            b"Content-Type: text/html junk\r\n\r\n",
            b"Content-Type: \r\n\r\n",
            b"Content-Type: (comment)\r\n\r\n",
        ] {
            assert_eq!(
                selection(source, Context::Normal).unwrap().content_type,
                ContentType::Default(DefaultType::TextPlain)
            );
        }
        let source = format!(
            "Content-Type: text/plain\r\nContent-Type: multipart/mixed {}x{}\r\n\r\n",
            "(".repeat(33),
            ")".repeat(33)
        );
        assert!(matches!(
            selection(source.as_bytes(), Context::Normal)
                .unwrap()
                .content_type,
            ContentType::Field(_)
        ));
    }
    #[test]
    fn prefix_requires_a_definitive_boundary_and_ranges_are_checked() {
        for source in [
            b"Content-Type: text/plain\r\n".as_slice(),
            b"Content-Type: text/plain",
            b"Content-Type: text/plain\r\nambiguous-name",
        ] {
            let mut refused = false;
            let mut cursor =
                Cursor::new(source, 0, 1000, Context::Normal, SourceEnd::Prefix).unwrap();
            let mut admission = work();
            for _ in 0..1000 {
                match cursor.poll(Tick(1), &mut admission) {
                    Ok(Status::Yield) => {}
                    Ok(Status::Complete) => panic!("prefix accepted"),
                    Err(error) => {
                        assert_eq!(error, Error::Truncated);
                        assert_eq!(cursor.poll(Tick(1), &mut work()), Err(error));
                        refused = true;
                        break;
                    }
                }
            }
            assert!(refused);
        }
        assert!(matches!(
            Cursor::new(b"ab", u64::MAX, 1000, Context::Normal, SourceEnd::Eof),
            Err(Error::InvalidRange)
        ));
        // Invalid fields definitively begin the body under the raw scanner's
        // recovery rule, even when only a resident prefix is available.
        for suffix in [b":bad".as_slice(), b"name\0", b"\rx"] {
            let mut source = b"Content-Type: text/plain\r\n".to_vec();
            let body = source.len() as u64;
            source.extend_from_slice(suffix);
            let mut cursor =
                Cursor::new(&source, 7, 1000, Context::Normal, SourceEnd::Prefix).unwrap();
            let mut admission = work();
            let mut complete = false;
            for _ in 0..1000 {
                if cursor.poll(Tick(1), &mut admission).unwrap() == Status::Complete {
                    let result = cursor.selection().unwrap().unwrap();
                    assert_eq!(result.end.body_start, 7 + body);
                    assert_eq!(result.end.header_bytes, body);
                    assert!(matches!(result.content_type, ContentType::Field(_)));
                    complete = true;
                    break;
                }
            }
            assert!(complete);
        }
        for source in [b":bad".as_slice(), b" unattached"] {
            let mut cursor =
                Cursor::new(source, 7, 1000, Context::Normal, SourceEnd::Prefix).unwrap();
            let mut admission = work();
            let mut complete = false;
            for _ in 0..1000 {
                if cursor.poll(Tick(1), &mut admission).unwrap() == Status::Complete {
                    let result = cursor.selection().unwrap().unwrap();
                    assert_eq!(result.end.body_start, 7);
                    assert_eq!(result.end.header_bytes, 0);
                    assert_eq!(
                        result.content_type,
                        ContentType::Default(DefaultType::TextPlain)
                    );
                    complete = true;
                    break;
                }
            }
            assert!(complete);
        }
        let mut cursor =
            Cursor::new(b"\r\nbody", 0, 1000, Context::Normal, SourceEnd::Prefix).unwrap();
        let mut admission = work();
        for _ in 0..1000 {
            if let Status::Complete = cursor.poll(Tick(1), &mut admission).unwrap() {
                let result = cursor.selection().unwrap().unwrap();
                assert_eq!(result.end.body_start, 2);
                return;
            }
        }
        panic!("separator not accepted")
    }
    #[test]
    fn each_job_work_cut_retires_the_selection() {
        let source = concat!(
            "Content-Type: bad;\r\n",
            "Content-Type: text/plain; name=\"a\\\"b\"\r\n",
            "Content-Disposition: attachment\r\n",
            "\r\n"
        )
        .as_bytes();
        let mut cursor = Cursor::new(source, 0, 1000, Context::Normal, SourceEnd::Eof).unwrap();
        let mut admission = work();
        let initial = admission.remaining();
        loop {
            if matches!(
                cursor.poll(Tick(1), &mut admission).unwrap(),
                Status::Complete
            ) {
                break;
            }
        }
        let used = JobCharge {
            io_bytes: initial.io_bytes - admission.remaining().io_bytes,
            records: initial.records - admission.remaining().records,
            ..JobCharge::default()
        };
        for bytes in [true, false] {
            let amount = if bytes { used.io_bytes } else { used.records };
            for limit in 0..amount {
                let mut cursor =
                    Cursor::new(source, 0, 1000, Context::Normal, SourceEnd::Eof).unwrap();
                let mut limits = used;
                if bytes {
                    limits.io_bytes = limit;
                } else {
                    limits.records = limit;
                }
                let mut admission = Meter::new(Deadline::after(Tick(0), 100).unwrap(), limits);
                let mut failed = false;
                for _ in 0..1000 {
                    match cursor.poll(Tick(1), &mut admission) {
                        Ok(Status::Yield) => {}
                        Ok(Status::Complete) => panic!("work cut accepted"),
                        Err(error) => {
                            let expected = if bytes { Stop::IoBytes } else { Stop::Records };
                            assert_eq!(error, Error::Work(expected));
                            let mut fresh = work();
                            let before = fresh.remaining();
                            assert_eq!(cursor.poll(Tick(1), &mut fresh), Err(error));
                            assert_eq!(fresh.remaining(), before);
                            failed = true;
                            break;
                        }
                    }
                }
                assert!(failed);
            }
        }
    }
    #[test]
    fn aggregate_turns_are_bounded_and_every_partial_limit_is_sticky() {
        let source = concat!(
            "Content-Type: bad;\r\n",
            "Content-Type: text/plain; name=\"a\\\"b\"\r\n",
            "Content-Disposition: attachment\r\n",
            "\r\n"
        )
        .as_bytes();
        let mut admission = work();
        let mut budget = HeaderBudget::new();
        let initial = (budget.source_bytes_remaining(), budget.steps_remaining());
        let mut cursor = Budgeted::new(
            source,
            0,
            1000,
            Context::Normal,
            SourceEnd::Eof,
            &mut admission,
            &mut budget,
        )
        .unwrap();
        loop {
            let before = (
                cursor.budget.source_bytes_remaining(),
                cursor.budget.steps_remaining(),
                cursor.work.remaining(),
            );
            let result = cursor.poll(Tick(1)).unwrap();
            assert!(before.0 - cursor.budget.source_bytes_remaining() <= 256);
            assert!(before.1 - cursor.budget.steps_remaining() <= 256);
            assert!(before.2.records - cursor.work.remaining().records <= 16);
            assert_eq!(before.2.output_bytes, cursor.work.remaining().output_bytes);
            if matches!(result, Status::Complete) {
                break;
            }
        }
        let used = (
            initial.0 - cursor.budget.source_bytes_remaining(),
            initial.1 - cursor.budget.steps_remaining(),
        );
        for bytes in [true, false] {
            let amount = if bytes { used.0 } else { used.1 };
            for limit in 0..amount {
                let mut admission = work();
                let mut budget = HeaderBudget::new();
                let mut credit = 0;
                let (visits, steps) = if bytes {
                    (budget.source_bytes_remaining() - limit, 0)
                } else {
                    (0, budget.steps_remaining() - limit)
                };
                budget
                    .charge(&mut admission, Tick(1), visits, steps, &mut credit)
                    .unwrap();
                let mut cursor = Budgeted::new(
                    source,
                    0,
                    1000,
                    Context::Normal,
                    SourceEnd::Eof,
                    &mut admission,
                    &mut budget,
                )
                .unwrap();
                let mut failed = false;
                for _ in 0..1000 {
                    match cursor.poll(Tick(1)) {
                        Ok(Status::Yield) => {}
                        Ok(Status::Complete) => panic!("aggregate cut accepted"),
                        Err(error) => {
                            assert_eq!(error, Error::InterpretationLimit);
                            let before = (
                                cursor.budget.source_bytes_remaining(),
                                cursor.budget.steps_remaining(),
                                cursor.work.remaining(),
                            );
                            assert_eq!(cursor.poll(Tick(1)), Err(error));
                            assert_eq!(
                                before,
                                (
                                    cursor.budget.source_bytes_remaining(),
                                    cursor.budget.steps_remaining(),
                                    cursor.work.remaining()
                                )
                            );
                            failed = true;
                            break;
                        }
                    }
                }
                assert!(failed);
            }
        }
    }
    #[test]
    fn completed_cache_is_inert_and_explicit_deadline_retires_metadata() {
        let source = b"Content-Type: text/plain\r\n\r\n";
        let mut admission = work();
        let mut budget = HeaderBudget::new();
        let mut cursor = Budgeted::new(
            source,
            0,
            1000,
            Context::Normal,
            SourceEnd::Eof,
            &mut admission,
            &mut budget,
        )
        .unwrap();
        assert_eq!(cursor.selection(), Ok(None));
        for _ in 0..1000 {
            if cursor.poll(Tick(1)).unwrap() == Status::Complete {
                let selected = cursor.selection().unwrap().unwrap();
                let before = (
                    cursor.work.remaining(),
                    cursor.budget.source_bytes_remaining(),
                    cursor.budget.steps_remaining(),
                );
                assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                assert_eq!(cursor.selection(), Ok(Some(selected)));
                assert_eq!(
                    before,
                    (
                        cursor.work.remaining(),
                        cursor.budget.source_bytes_remaining(),
                        cursor.budget.steps_remaining()
                    )
                );
                assert_eq!(
                    cursor.check_deadline(Tick(100)),
                    Err(Error::Work(Stop::Deadline))
                );
                assert_eq!(cursor.selection(), Err(Error::Work(Stop::Deadline)));
                assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
                return;
            }
            assert_eq!(cursor.selection(), Ok(None));
        }
        panic!("not complete")
    }
    #[test]
    fn a_late_raw_header_limit_retires_a_valid_candidate() {
        let source = b"Content-Type: text/plain\r\nX-Long: aaaaaa\r\n\r\n";
        let mut cursor = Cursor::new(source, 0, 30, Context::Normal, SourceEnd::Eof).unwrap();
        let mut admission = work();
        for _ in 0..1000 {
            match cursor.poll(Tick(1), &mut admission) {
                Ok(Status::Yield) => {}
                Ok(Status::Complete) => panic!("accepted"),
                Err(error) => {
                    assert_eq!(error, Error::Headers(mime_headers::Error::HeaderLimit));
                    assert_eq!(cursor.selection(), Err(error));
                    assert_eq!(cursor.poll(Tick(1), &mut work()), Err(error));
                    return;
                }
            }
        }
        panic!("not refused")
    }
    #[test]
    fn long_unicode_values_and_folds_preserve_raw_heads_with_fixed_turns() {
        let source = format!(
            concat!(
                "cOnTeNt-TyPe: ({} ) TeXt (x)/\r\n",
                " (y)PlAiN; name=\"{}\"\r\n",
                "Content-Transfer-Encoding: (x)X-CuStOm\r\n",
                "Content-Transfer-Encoding: base64\r\n\r\nbody"
            ),
            "🐈".repeat(4096),
            "🐈".repeat(8192)
        );
        let result = selection(source.as_bytes(), Context::Normal).unwrap();
        let ContentType::Field(selected) = result.content_type else {
            panic!("default")
        };
        let start = (selected.field.value_start - 100) as usize;
        assert_eq!(
            &source.as_bytes()[start + selected.head.first.start..start + selected.head.first.end],
            b"TeXt"
        );
        let second = selected.head.second.unwrap();
        assert_eq!(
            &source.as_bytes()[start + second.start..start + second.end],
            b"PlAiN"
        );
        let transfer = result.transfer_encoding.unwrap();
        let start = (transfer.field.value_start - 100) as usize;
        assert_eq!(
            &source.as_bytes()[start + transfer.head.first.start..start + transfer.head.first.end],
            b"X-CuStOm"
        );
        let mut admission = work();
        let mut budget = HeaderBudget::new();
        let mut cursor = Budgeted::new(
            source.as_bytes(),
            100,
            source.len() as u64,
            Context::Normal,
            SourceEnd::Eof,
            &mut admission,
            &mut budget,
        )
        .unwrap();
        assert!(std::mem::size_of_val(&cursor) <= 1024);
        for _ in 0..100_000 {
            let before = (
                cursor.work.remaining(),
                cursor.budget.source_bytes_remaining(),
                cursor.budget.steps_remaining(),
            );
            let status = cursor.poll(Tick(1)).unwrap();
            assert!(before.0.io_bytes - cursor.work.remaining().io_bytes <= 256);
            assert!(before.0.records - cursor.work.remaining().records <= 16);
            assert!(before.1 - cursor.budget.source_bytes_remaining() <= 256);
            assert!(before.2 - cursor.budget.steps_remaining() <= 256);
            if status == Status::Complete {
                assert_eq!(cursor.selection(), Ok(Some(result)));
                return;
            }
        }
        panic!("long field did not complete")
    }
    #[test]
    fn budgeted_job_cuts_include_prepaid_record_credit() {
        let source = b"Content-Type: bad;\r\nContent-Type: text/plain; name=x\r\n\r\n";
        let mut admission = work();
        let initial = admission.remaining();
        let mut budget = HeaderBudget::new();
        let mut cursor = Budgeted::new(
            source,
            0,
            1000,
            Context::Normal,
            SourceEnd::Eof,
            &mut admission,
            &mut budget,
        )
        .unwrap();
        loop {
            if cursor.poll(Tick(1)).unwrap() == Status::Complete {
                break;
            }
        }
        let remaining = cursor.work.remaining();
        let used = JobCharge {
            io_bytes: initial.io_bytes - remaining.io_bytes,
            records: initial.records - remaining.records,
            ..JobCharge::default()
        };
        for bytes in [true, false] {
            let amount = if bytes { used.io_bytes } else { used.records };
            for limit in 0..amount {
                let mut caps = used;
                if bytes {
                    caps.io_bytes = limit;
                } else {
                    caps.records = limit;
                }
                let mut admission = Meter::new(Deadline::after(Tick(0), 100).unwrap(), caps);
                let mut budget = HeaderBudget::new();
                let mut cursor = Budgeted::new(
                    source,
                    0,
                    1000,
                    Context::Normal,
                    SourceEnd::Eof,
                    &mut admission,
                    &mut budget,
                )
                .unwrap();
                let mut failed = false;
                for _ in 0..1000 {
                    match cursor.poll(Tick(1)) {
                        Ok(Status::Yield) => {}
                        Ok(Status::Complete) => panic!("prepaid work cut admitted"),
                        Err(error) => {
                            let expected = if bytes { Stop::IoBytes } else { Stop::Records };
                            assert_eq!(error, Error::Work(expected));
                            let before = (
                                cursor.work.remaining(),
                                cursor.budget.steps_remaining(),
                                cursor.budget.source_bytes_remaining(),
                            );
                            assert_eq!(cursor.poll(Tick(1)), Err(error));
                            assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                            assert_eq!(cursor.selection(), Err(error));
                            assert_eq!(
                                before,
                                (
                                    cursor.work.remaining(),
                                    cursor.budget.steps_remaining(),
                                    cursor.budget.source_bytes_remaining()
                                )
                            );
                            failed = true;
                            break;
                        }
                    }
                }
                assert!(failed);
            }
        }
    }
    #[test]
    fn maximum_short_unrelated_headers_do_not_exhaust_interpretation() {
        let mut source = "a:\n".repeat((1024 * 1024) / 3).into_bytes();
        source.push(b'\n');
        let mut admission = work();
        let mut budget = HeaderBudget::new();
        let mut cursor = Budgeted::new(
            &source,
            0,
            1024 * 1024,
            Context::Normal,
            SourceEnd::Eof,
            &mut admission,
            &mut budget,
        )
        .unwrap();
        for _ in 0..2_000_000 {
            if cursor.poll(Tick(1)).unwrap() == Status::Complete {
                let result = cursor.selection().unwrap().unwrap();
                assert_eq!(result.end.body_start, source.len() as u64);
                assert_eq!(result.end.header_bytes, source.len() as u64 - 1);
                assert_eq!(
                    result.content_type,
                    ContentType::Default(DefaultType::TextPlain)
                );
                return;
            }
        }
        panic!("short headers did not complete");
    }
}
