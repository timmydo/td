//! Retained selected MIME heads, charset and filename from a complete entity.
use crate::{
    admission::work::{Charge, Meter},
    header_select::SourceEnd,
    mime_filename::{self, Fields},
    mime_metadata::{self, ContentType, DefaultType, Selected},
    mime_parameter::protocol,
    nfc::{self, HeaderBudget, Scratch},
    ports::Tick,
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Metadata(mime_metadata::Error),
    Charset(protocol::Error),
    Filename(mime_filename::Error),
    Admission(nfc::Error),
    OutputCapacity,
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Metadata(e) => write!(f, "MIME part headers: {e}"),
            Self::Charset(e) => write!(f, "MIME part charset: {e}"),
            Self::Filename(e) => write!(f, "MIME part filename: {e}"),
            Self::Admission(e) => write!(f, "MIME part admission: {e}"),
            Self::OutputCapacity => f.write_str("MIME part head capacity"),
            Self::InvalidState => f.write_str("invalid MIME part header state"),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Metadata(e) => Some(e),
            Self::Charset(e) => Some(e),
            Self::Filename(e) => Some(e),
            Self::Admission(e) => Some(e),
            _ => None,
        }
    }
}
/// Conservative head capacity from recognized raw entity-header bytes.
/// Includes the fourteen-byte digest default even for an empty section.
pub const fn heads_capacity_bound(header_bytes: usize) -> Option<usize> {
    header_bytes.checked_add(14)
}
/// Complete authorized entity bounds and context are supplied by the caller.
#[derive(Clone, Copy)]
pub struct Entity<'a> {
    pub source: &'a [u8],
    pub base: u64,
    pub source_end: SourceEnd,
    pub header_limit: u64,
    pub context: mime_metadata::Context,
}
/// Separate caller-reserved windows; capacity is never expanded by parsing.
pub struct Backing<'w> {
    pub heads: &'w mut [u8],
    pub charset: &'w mut [u8],
    pub filename: &'w mut [u8],
}
/// Passive completed projection. It grants no source, blob or publication authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct View<'w> {
    pub content_type: &'w [u8],
    pub disposition: Option<&'w [u8]>,
    pub charset: Option<&'w [u8]>,
    pub charset_end: Option<protocol::End>,
    pub filename: Option<&'w [u8]>,
    pub filename_end: mime_filename::End,
    pub body_start: u64,
    pub header_bytes: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Complete,
}
// Live child phases are exclusive; selector storage remains separately reserved.
#[allow(clippy::large_enum_variant)]
enum Owner<'a, 'w> {
    Budgets(&'w mut Meter, &'w mut HeaderBudget, &'w mut Scratch),
    Charset(protocol::Cursor<'a, 'w>, &'w mut Scratch),
    Filename(mime_filename::Cursor<'a, 'w>),
    Retired,
}
#[derive(Clone, Copy)]
enum Phase {
    Select,
    Heads,
    Charset,
    Filename,
    Complete,
}
/// Replays selected entity headers under the same original job/header budgets.
/// Raw bytes and default context must come from the caller's authorized entity.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_part_headers::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_part_headers::Cursor<'_, '_>>();
/// ```
pub struct Cursor<'a, 'w> {
    source: &'a [u8],
    base: u64,
    selector: Option<mime_metadata::Cursor<'a>>,
    owner: Owner<'a, 'w>,
    phase: Phase,
    credit: u8,
    fields: Fields<'a>,
    tokens: [Option<&'a [u8]>; 4],
    token: usize,
    offset: usize,
    heads: &'w mut [u8],
    used: usize,
    type_len: usize,
    disposition: bool,
    charset_output: Option<&'w mut [u8]>,
    filename_output: Option<&'w mut [u8]>,
    charset: Option<protocol::Retained<'w>>,
    filename: Option<mime_filename::Retained<'w>>,
    end: Option<crate::mime_headers::End>,
    failure: Option<Error>,
}
impl<'a, 'w> Cursor<'a, 'w> {
    fn metadata_error(error: mime_metadata::Error) -> Error {
        match error {
            mime_metadata::Error::Work(stop) => Error::Admission(nfc::Error::Work(stop)),
            mime_metadata::Error::InterpretationLimit => {
                Error::Admission(nfc::Error::InterpretationLimit)
            }
            other => Error::Metadata(other),
        }
    }
    fn charset_error(error: protocol::Error) -> Error {
        match error {
            protocol::Error::Parameter(crate::mime_parameter::Error::Work(stop)) => {
                Error::Admission(nfc::Error::Work(stop))
            }
            protocol::Error::Parameter(crate::mime_parameter::Error::InterpretationLimit) => {
                Error::Admission(nfc::Error::InterpretationLimit)
            }
            other => Error::Charset(other),
        }
    }
    fn filename_error(error: mime_filename::Error) -> Error {
        match error {
            mime_filename::Error::Admission(error) => Error::Admission(error),
            other => Error::Filename(other),
        }
    }
    pub fn new(
        entity: Entity<'a>,
        output: Backing<'w>,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
        scratch: &'w mut Scratch,
    ) -> Result<Self, Error> {
        let Entity {
            source,
            base,
            source_end,
            header_limit,
            context,
        } = entity;
        if source_end != SourceEnd::Eof {
            return Err(Error::Metadata(mime_metadata::Error::Truncated));
        }
        Ok(Self {
            source,
            base,
            selector: Some(
                mime_metadata::Cursor::new(source, base, header_limit, context, source_end)
                    .map_err(Self::metadata_error)?,
            ),
            owner: Owner::Budgets(work, budget, scratch),
            phase: Phase::Select,
            credit: 0,
            fields: Fields {
                content_type: None,
                disposition: None,
            },
            tokens: [None; 4],
            token: 0,
            offset: 0,
            heads: output.heads,
            used: 0,
            type_len: 0,
            disposition: false,
            charset_output: Some(output.charset),
            filename_output: Some(output.filename),
            charset: None,
            filename: None,
            end: None,
            failure: None,
        })
    }
    fn range(&self, start: u64, end: u64) -> Result<&'a [u8], Error> {
        let start = usize::try_from(start.checked_sub(self.base).ok_or(Error::InvalidState)?)
            .map_err(|_| Error::InvalidState)?;
        let end = usize::try_from(end.checked_sub(self.base).ok_or(Error::InvalidState)?)
            .map_err(|_| Error::InvalidState)?;
        self.source.get(start..end).ok_or(Error::InvalidState)
    }
    fn field(&self, selected: Selected) -> Result<&'a [u8], Error> {
        self.range(selected.field.value_start, selected.field.value_end)
    }
    fn head(&self, selected: Selected, second: bool) -> Result<&'a [u8], Error> {
        let extent = if second {
            selected.head.second.ok_or(Error::InvalidState)?
        } else {
            selected.head.first
        };
        self.field(selected)?
            .get(extent.start..extent.end)
            .ok_or(Error::InvalidState)
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
            self.owner = Owner::Retired;
            self.selector = None;
            self.charset = None;
            self.filename = None;
            self.end = None;
        }
        result
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = match &mut self.owner {
            Owner::Budgets(work, budget, _) => budget
                .charge(work, now, 0, 0, &mut self.credit)
                .map_err(Error::Admission),
            Owner::Charset(cursor, _) => cursor.check_deadline(now).map_err(Self::charset_error),
            Owner::Filename(cursor) => cursor.check_deadline(now).map_err(Self::filename_error),
            Owner::Retired => Err(Error::InvalidState),
        };
        self.outcome(result)
    }
    pub fn value(&self) -> Option<View<'_>> {
        if self.failure.is_some() || !matches!(self.phase, Phase::Complete) {
            return None;
        }
        let end = self.end?;
        let filename = self.filename.as_ref()?;
        Some(View {
            content_type: self.heads.get(..self.type_len)?,
            disposition: if self.disposition {
                Some(self.heads.get(self.type_len..self.used)?)
            } else {
                None
            },
            charset: self.charset.as_ref().and_then(|value| value.value()),
            charset_end: self.charset.as_ref().map(|value| value.end),
            filename: filename.value(),
            filename_end: filename.end,
            body_start: end.body_start,
            header_bytes: end.header_bytes,
        })
    }
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<
        (
            View<'w>,
            &'w mut Meter,
            &'w mut HeaderBudget,
            &'w mut Scratch,
        ),
        Error,
    > {
        self.check_deadline(now)?;
        if !matches!(self.phase, Phase::Complete) {
            return Err(Error::InvalidState);
        }
        let end = self.end.ok_or(Error::InvalidState)?;
        let filename = self.filename.ok_or(Error::InvalidState)?;
        let Owner::Budgets(work, budget, scratch) = self.owner else {
            return Err(Error::InvalidState);
        };
        let heads: &'w [u8] = self.heads;
        let view = View {
            content_type: heads.get(..self.type_len).ok_or(Error::InvalidState)?,
            disposition: if self.disposition {
                Some(
                    heads
                        .get(self.type_len..self.used)
                        .ok_or(Error::InvalidState)?,
                )
            } else {
                None
            },
            charset: self.charset.as_ref().and_then(|value| value.value()),
            charset_end: self.charset.as_ref().map(|value| value.end),
            filename: filename.value(),
            filename_end: filename.end,
            body_start: end.body_start,
            header_bytes: end.header_bytes,
        };
        Ok((view, work, budget, scratch))
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.phase, Phase::Complete) {
            return Ok(Status::Complete);
        }
        let result = self.step(now);
        self.outcome(result)
    }
    fn step(&mut self, now: Tick) -> Result<Status, Error> {
        self.check_deadline(now)?;
        match self.phase {
            Phase::Select => {
                let Owner::Budgets(work, budget, _) = &mut self.owner else {
                    return Err(Error::InvalidState);
                };
                let selector = self.selector.as_mut().ok_or(Error::InvalidState)?;
                if selector
                    .poll_in_context(now, work, budget, &mut self.credit)
                    .map_err(Self::metadata_error)?
                    == mime_metadata::Status::Complete
                {
                    let selected = selector
                        .selection()
                        .map_err(Self::metadata_error)?
                        .ok_or(Error::InvalidState)?;
                    self.end = Some(selected.end);
                    match selected.content_type {
                        ContentType::Default(value) => {
                            *self.tokens.get_mut(0).ok_or(Error::InvalidState)? =
                                Some(match value {
                                    DefaultType::TextPlain => b"text/plain",
                                    DefaultType::MessageRfc822 => b"message/rfc822",
                                })
                        }
                        ContentType::Field(value) => {
                            self.fields.content_type = Some(self.field(value)?);
                            *self.tokens.get_mut(0).ok_or(Error::InvalidState)? =
                                Some(self.head(value, false)?);
                            *self.tokens.get_mut(1).ok_or(Error::InvalidState)? = Some(b"/");
                            *self.tokens.get_mut(2).ok_or(Error::InvalidState)? =
                                Some(self.head(value, true)?);
                        }
                    }
                    if let Some(value) = selected.content_disposition {
                        self.fields.disposition = Some(self.field(value)?);
                        *self.tokens.get_mut(3).ok_or(Error::InvalidState)? =
                            Some(self.head(value, false)?);
                        self.disposition = true;
                    }
                    self.selector = None;
                    self.phase = Phase::Heads;
                }
            }
            Phase::Heads => {
                let Owner::Budgets(work, budget, _) = &mut self.owner else {
                    return Err(Error::InvalidState);
                };
                budget
                    .charge(work, now, 0, 1, &mut self.credit)
                    .map_err(Error::Admission)?;
                if self.token == 3 && self.offset == 0 {
                    self.type_len = self.used;
                }
                let Some(source) = self.tokens.get(self.token).copied().flatten() else {
                    if self.token < 3 {
                        self.token += 1;
                    } else {
                        self.phase = Phase::Charset;
                    }
                    return Ok(Status::Yield);
                };
                let end = self.offset.saturating_add(32).min(source.len());
                let bytes = source.get(self.offset..end).ok_or(Error::InvalidState)?;
                let next = self
                    .used
                    .checked_add(bytes.len())
                    .ok_or(Error::OutputCapacity)?;
                let target = self
                    .heads
                    .get_mut(self.used..next)
                    .ok_or(Error::OutputCapacity)?;
                budget
                    .charge(
                        work,
                        now,
                        bytes.len() as u64,
                        bytes.len() as u64,
                        &mut self.credit,
                    )
                    .map_err(Error::Admission)?;
                work.charge(
                    now,
                    Charge {
                        output_bytes: bytes.len() as u64,
                        ..Charge::default()
                    },
                )
                .map_err(nfc::Error::Work)
                .map_err(Error::Admission)?;
                for (out, input) in target.iter_mut().zip(bytes) {
                    *out = input.to_ascii_lowercase();
                }
                self.used = next;
                self.offset = end;
                if end == source.len() {
                    self.offset = 0;
                    if self.token < 3 {
                        self.token += 1;
                    } else {
                        self.phase = Phase::Charset;
                    }
                }
            }
            Phase::Charset => match &mut self.owner {
                Owner::Budgets(work, budget, _) => {
                    budget
                        .charge(work, now, 0, 1, &mut self.credit)
                        .map_err(Error::Admission)?;
                    if let Some(source) = self.fields.content_type {
                        let Owner::Budgets(work, budget, scratch) =
                            std::mem::replace(&mut self.owner, Owner::Retired)
                        else {
                            return Err(Error::InvalidState);
                        };
                        let output = self.charset_output.take().ok_or(Error::InvalidState)?;
                        self.owner = Owner::Charset(
                            protocol::Cursor::new(
                                source,
                                protocol::Purpose::Charset,
                                output,
                                work,
                                budget,
                            ),
                            scratch,
                        );
                    } else {
                        self.phase = Phase::Filename;
                    }
                }
                Owner::Charset(cursor, _) => {
                    if matches!(
                        cursor.poll(now).map_err(Self::charset_error)?,
                        protocol::Status::Complete(_)
                    ) {
                        let Owner::Charset(cursor, scratch) =
                            std::mem::replace(&mut self.owner, Owner::Retired)
                        else {
                            return Err(Error::InvalidState);
                        };
                        let (value, work, budget) =
                            cursor.finish(now).map_err(Self::charset_error)?;
                        self.charset = Some(value);
                        self.owner = Owner::Budgets(work, budget, scratch);
                        self.phase = Phase::Filename;
                    }
                }
                _ => return Err(Error::InvalidState),
            },
            Phase::Filename => match &mut self.owner {
                Owner::Budgets(work, budget, _) => {
                    budget
                        .charge(work, now, 0, 1, &mut self.credit)
                        .map_err(Error::Admission)?;
                    let Owner::Budgets(work, budget, scratch) =
                        std::mem::replace(&mut self.owner, Owner::Retired)
                    else {
                        return Err(Error::InvalidState);
                    };
                    let output = self.filename_output.take().ok_or(Error::InvalidState)?;
                    self.owner = Owner::Filename(mime_filename::Cursor::new(
                        self.fields,
                        output,
                        work,
                        budget,
                        scratch,
                    ));
                }
                Owner::Filename(cursor) => {
                    if matches!(
                        cursor.poll(now).map_err(Self::filename_error)?,
                        mime_filename::Status::Complete(_)
                    ) {
                        let Owner::Filename(cursor) =
                            std::mem::replace(&mut self.owner, Owner::Retired)
                        else {
                            return Err(Error::InvalidState);
                        };
                        let (value, work, budget, scratch) =
                            cursor.finish(now).map_err(Self::filename_error)?;
                        self.filename = Some(value);
                        self.owner = Owner::Budgets(work, budget, scratch);
                        self.phase = Phase::Complete;
                        return Ok(Status::Complete);
                    }
                }
                _ => return Err(Error::InvalidState),
            },
            Phase::Complete => return Ok(Status::Complete),
        }
        Ok(Status::Yield)
    }
}
const _: () = assert!(
    std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 16 * 1024
);

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{admission::work::Stop, ports::Deadline};
    fn meter() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000_000,
                records: 100_000_000,
                output_bytes: 100_000_000,
                ..Charge::default()
            },
        )
    }
    fn drain(cursor: &mut Cursor<'_, '_>) -> Result<(), Error> {
        for _ in 0..100_000 {
            if cursor.poll(Tick(1))? == Status::Complete {
                return Ok(());
            }
            assert!(cursor.value().is_none());
        }
        panic!("part header cursor did not complete")
    }
    fn backing<'w>(
        heads: &'w mut [u8],
        charset: &'w mut [u8],
        filename: &'w mut [u8],
    ) -> Backing<'w> {
        Backing {
            heads,
            charset,
            filename,
        }
    }
    #[test]
    fn selected_heads_charset_name_and_original_handoff() {
        let source = concat!(
            "Content-Type: broken;\r\n",
            "Content-Type: (c) TeXT / HTmL; charset=\"uTf-8\";name=type\r\n",
            "Content-Type: application/ignored\r\n",
            "Content-Disposition: ATTACHMENT;filename*=utf-8''e%CC%81\r\n",
            "Content-Disposition: inline;filename=later\r\n\r\nBODY"
        )
        .as_bytes();
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let identity = (
            std::ptr::from_ref(&work),
            std::ptr::from_ref(&budget),
            std::ptr::from_ref(&scratch),
        );
        let mut heads = [0; 64];
        let mut charset = [0; 16];
        let mut name = [0; 64];
        let mut cursor = Cursor::new(
            Entity {
                source,
                base: 100,
                source_end: SourceEnd::Eof,
                header_limit: 1024,
                context: mime_metadata::Context::Normal,
            },
            backing(&mut heads, &mut charset, &mut name),
            &mut work,
            &mut budget,
            &mut scratch,
        )
        .unwrap();
        drain(&mut cursor).unwrap();
        let value = cursor.value().unwrap();
        assert_eq!(value.content_type, b"text/html");
        assert_eq!(value.disposition, Some(b"attachment".as_slice()));
        assert_eq!(value.charset, Some(b"uTf-8".as_slice()));
        assert_eq!(
            value.charset_end.unwrap().known_charset,
            Some(crate::mime_charset::Charset::Utf8)
        );
        assert_eq!(value.filename, Some("é".as_bytes()));
        assert_eq!(value.body_start, 100 + source.len() as u64 - 4);
        assert_eq!(value.header_bytes, source.len() as u64 - 6);
        assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
        let (value, work, budget, scratch) = cursor.finish(Tick(1)).unwrap();
        assert_eq!(value.content_type, b"text/html");
        assert_eq!(
            (
                std::ptr::from_ref(&*work),
                std::ptr::from_ref(&*budget),
                std::ptr::from_ref(&*scratch)
            ),
            identity
        );
        assert!(budget.source_bytes_remaining() < 16 * 1024 * 1024);
        work.charge(Tick(1), Charge::default()).unwrap();
        assert_eq!(value.filename, Some("é".as_bytes()));
    }
    #[test]
    fn defaults_absence_invalid_charset_empty_name_and_long_heads() {
        let long = format!(
            "Content-Type: X{}/Y{}\n\n",
            "A".repeat(600),
            "B".repeat(600)
        );
        type Case<'a> = (
            &'a [u8],
            mime_metadata::Context,
            &'a [u8],
            Option<&'a [u8]>,
            Option<&'a [u8]>,
            Option<protocol::Value>,
        );
        const INVALID_CHARSET: &[u8] = concat!(
            "Content-Type:text/plain;name=x;charset*=utf-8''bad%20value\n",
            "Content-Disposition:inline;filename=\"\"\n\n"
        )
        .as_bytes();
        const TYPE_NAME: &[u8] = concat!(
            "Content-Type:text/plain;name=type\n",
            "Content-Disposition:attachment\n\n"
        )
        .as_bytes();
        let cases: &[Case<'_>] = &[
            (
                b"\n",
                mime_metadata::Context::Normal,
                b"text/plain",
                None,
                None,
                None,
            ),
            (
                b"Content-Type: invalid;\n\n",
                mime_metadata::Context::DigestChild,
                b"message/rfc822",
                None,
                None,
                None,
            ),
            (
                INVALID_CHARSET,
                mime_metadata::Context::Normal,
                b"text/plain",
                Some(b""),
                None,
                Some(protocol::Value::Invalid),
            ),
            (
                b"Content-Type:text/plain;charset=X-UNKNOWN\n\n",
                mime_metadata::Context::Normal,
                b"text/plain",
                None,
                Some(b"X-UNKNOWN"),
                Some(protocol::Value::Present),
            ),
            (
                TYPE_NAME,
                mime_metadata::Context::Normal,
                b"text/plain",
                Some(b"type"),
                None,
                Some(protocol::Value::Absent),
            ),
        ];
        for &(source, context, typ, name, charset, state) in cases {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut scratch = Scratch::new();
            let mut h = [0; 64];
            let mut c = [0; 32];
            let mut n = [0; 64];
            let mut cursor = Cursor::new(
                Entity {
                    source,
                    base: 0,
                    source_end: SourceEnd::Eof,
                    header_limit: 1024,
                    context,
                },
                backing(&mut h, &mut c, &mut n),
                &mut work,
                &mut budget,
                &mut scratch,
            )
            .unwrap();
            drain(&mut cursor).unwrap();
            let value = cursor.value().unwrap();
            assert_eq!(value.content_type, typ);
            assert_eq!(value.filename, name);
            assert_eq!(value.charset, charset);
            assert_eq!(value.charset_end.map(|end| end.value), state);
            if charset == Some(b"X-UNKNOWN") {
                assert_eq!(value.charset_end.unwrap().known_charset, None);
            }
        }
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut h = [0; 2048];
        let mut c = [];
        let mut n = [];
        let mut cursor = Cursor::new(
            Entity {
                source: long.as_bytes(),
                base: 0,
                source_end: SourceEnd::Eof,
                header_limit: 2048,
                context: mime_metadata::Context::Normal,
            },
            backing(&mut h, &mut c, &mut n),
            &mut work,
            &mut budget,
            &mut scratch,
        )
        .unwrap();
        drain(&mut cursor).unwrap();
        assert_eq!(
            cursor.value().unwrap().content_type,
            format!("x{}/y{}", "a".repeat(600), "b".repeat(600)).as_bytes()
        );
    }
    fn resource(error: Error) -> Option<nfc::Error> {
        match error {
            Error::Admission(error) => Some(error),
            _ => None,
        }
    }

    #[test]
    fn every_original_resource_cut_is_sticky_without_partial_result() {
        let source = b"Content-Type:TEXT/PLAIN;charset=utf-8;name=x\n\n";
        let mut work = meter();
        let before = work.remaining();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let initial = (budget.source_bytes_remaining(), budget.steps_remaining());
        let mut h = [0; 32];
        let mut c = [0; 16];
        let mut n = [0; 8];
        let mut cursor = Cursor::new(
            Entity {
                source,
                base: 0,
                source_end: SourceEnd::Eof,
                header_limit: 1024,
                context: mime_metadata::Context::Normal,
            },
            backing(&mut h, &mut c, &mut n),
            &mut work,
            &mut budget,
            &mut scratch,
        )
        .unwrap();
        drain(&mut cursor).unwrap();
        cursor.finish(Tick(1)).unwrap();
        let totals = [
            initial.0 - budget.source_bytes_remaining(),
            initial.1 - budget.steps_remaining(),
            before.io_bytes - work.remaining().io_bytes,
            before.records - work.remaining().records,
            before.output_bytes - work.remaining().output_bytes,
        ];
        for (flavor, total) in totals.into_iter().enumerate() {
            assert!(total > 0);
            for cap in 0..total {
                let mut work = meter();
                let mut budget = HeaderBudget::new();
                if flavor < 2 {
                    let bytes = if flavor == 0 {
                        budget.source_bytes_remaining() - cap
                    } else {
                        0
                    };
                    let steps = if flavor == 1 {
                        budget.steps_remaining() - cap
                    } else {
                        0
                    };
                    budget
                        .charge(&mut work, Tick(1), bytes, steps, &mut 0)
                        .unwrap();
                } else {
                    let mut allowance = work.remaining();
                    match flavor {
                        2 => allowance.io_bytes = cap,
                        3 => allowance.records = cap,
                        4 => allowance.output_bytes = cap,
                        _ => panic!("invalid cut"),
                    };
                    work = Meter::new(Deadline::after(Tick(0), 100).unwrap(), allowance);
                }
                let mut scratch = Scratch::new();
                let mut h = [0; 32];
                let mut c = [0; 16];
                let mut n = [0; 8];
                let mut cursor = Cursor::new(
                    Entity {
                        source,
                        base: 0,
                        source_end: SourceEnd::Eof,
                        header_limit: 1024,
                        context: mime_metadata::Context::Normal,
                    },
                    backing(&mut h, &mut c, &mut n),
                    &mut work,
                    &mut budget,
                    &mut scratch,
                )
                .unwrap();
                let error = drain(&mut cursor).unwrap_err();
                let expected = match flavor {
                    0 | 1 => nfc::Error::InterpretationLimit,
                    2 => nfc::Error::Work(Stop::IoBytes),
                    3 => nfc::Error::Work(Stop::Records),
                    4 => nfc::Error::Work(Stop::OutputBytes),
                    _ => panic!("invalid cut"),
                };
                assert_eq!(resource(error), Some(expected));
                assert_eq!(cursor.value(), None);
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                let result = cursor.finish(Tick(1));
                assert!(matches!(result, Err(e) if e == error));
            }
        }
    }
    #[test]
    fn capacity_prefix_nesting_and_every_deadline_turn() {
        let source =
            b"Content-Type:TEXT/PLAIN;charset=utf-8\nContent-Disposition:inline;filename=x\n\n";
        let mut count = 0;
        for cut in 0..1000 {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut scratch = Scratch::new();
            let mut h = [0; 32];
            let mut c = [0; 16];
            let mut n = [0; 8];
            let mut cursor = Cursor::new(
                Entity {
                    source,
                    base: 0,
                    source_end: SourceEnd::Eof,
                    header_limit: 1024,
                    context: mime_metadata::Context::Normal,
                },
                backing(&mut h, &mut c, &mut n),
                &mut work,
                &mut budget,
                &mut scratch,
            )
            .unwrap();
            let mut completed = false;
            for _ in 0..cut {
                if cursor.poll(Tick(1)).unwrap() == Status::Complete {
                    completed = true;
                    break;
                }
            }
            if completed {
                count = cut;
                assert!(
                    matches!(cursor.finish(Tick(100)),Err(e) if resource(e)==Some(nfc::Error::Work(Stop::Deadline)))
                );
                break;
            }
            let error = cursor.check_deadline(Tick(100)).unwrap_err();
            assert_eq!(resource(error), Some(nfc::Error::Work(Stop::Deadline)));
            assert_eq!(cursor.value(), None);
            assert_eq!(cursor.poll(Tick(1)), Err(error));
        }
        assert!(count > 0);
        for flavor in 0..3 {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut scratch = Scratch::new();
            let mut h = [0; 32];
            let mut c = [0; 16];
            let mut n = [0; 8];
            let mut cursor = Cursor::new(
                Entity {
                    source,
                    base: 0,
                    source_end: SourceEnd::Eof,
                    header_limit: 1024,
                    context: mime_metadata::Context::Normal,
                },
                backing(
                    if flavor == 0 { &mut [] } else { &mut h },
                    if flavor == 1 { &mut [] } else { &mut c },
                    if flavor == 2 { &mut [] } else { &mut n },
                ),
                &mut work,
                &mut budget,
                &mut scratch,
            )
            .unwrap();
            let error = drain(&mut cursor).unwrap_err();
            let expected = match flavor {
                0 => Error::OutputCapacity,
                1 => Error::Charset(protocol::Error::OutputCapacity),
                2 => Error::Filename(mime_filename::Error::OutputCapacity),
                _ => panic!("invalid window"),
            };
            assert_eq!(error, expected);
            assert_eq!(cursor.value(), None);
        }
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut h = [0; 32];
        let mut c = [0; 16];
        let mut n = [0; 8];
        assert!(matches!(
            Cursor::new(
                Entity {
                    source,
                    base: 0,
                    source_end: SourceEnd::Prefix,
                    header_limit: 1024,
                    context: mime_metadata::Context::Normal
                },
                backing(&mut h, &mut c, &mut n),
                &mut work,
                &mut budget,
                &mut scratch
            ),
            Err(Error::Metadata(mime_metadata::Error::Truncated))
        ));
        let nested = format!(
            "Content-Type:{}x{} text/plain\n\n",
            "(".repeat(33),
            ")".repeat(33)
        );
        let mut cursor = Cursor::new(
            Entity {
                source: nested.as_bytes(),
                base: 0,
                source_end: SourceEnd::Eof,
                header_limit: 1024,
                context: mime_metadata::Context::Normal,
            },
            backing(&mut h, &mut c, &mut n),
            &mut work,
            &mut budget,
            &mut scratch,
        )
        .unwrap();
        assert_eq!(
            drain(&mut cursor),
            Err(Error::Metadata(mime_metadata::Error::NestingLimit))
        );
    }
    #[test]
    fn exact_window_edges_and_forwarded_family_diagnostics() {
        let source = concat!(
            "Content-Type:TEXT/PLAIN;charset=UTF-8\n",
            "Content-Disposition:ATTACHMENT;filename*=utf-8''e%CC%81\n\n"
        )
        .as_bytes();
        for shortened in 0..=3 {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut scratch = Scratch::new();
            let mut heads = [0; 20];
            let mut charset = [0; 5];
            let mut name = [0; 2];
            let output = Backing {
                heads: if shortened == 1 {
                    &mut heads[..19]
                } else {
                    &mut heads
                },
                charset: if shortened == 2 {
                    &mut charset[..4]
                } else {
                    &mut charset
                },
                filename: if shortened == 3 {
                    &mut name[..1]
                } else {
                    &mut name
                },
            };
            let mut cursor = Cursor::new(
                Entity {
                    source,
                    base: 0,
                    source_end: SourceEnd::Eof,
                    header_limit: 1024,
                    context: mime_metadata::Context::Normal,
                },
                output,
                &mut work,
                &mut budget,
                &mut scratch,
            )
            .unwrap();
            if shortened == 0 {
                drain(&mut cursor).unwrap();
                let value = cursor.finish(Tick(1)).unwrap().0;
                assert_eq!(value.content_type, b"text/plain");
                assert_eq!(value.disposition, Some(b"attachment".as_slice()));
                assert_eq!(value.charset, Some(b"UTF-8".as_slice()));
                assert_eq!(value.filename, Some("é".as_bytes()));
            } else {
                let expected = match shortened {
                    1 => Error::OutputCapacity,
                    2 => Error::Charset(protocol::Error::OutputCapacity),
                    3 => Error::Filename(mime_filename::Error::OutputCapacity),
                    _ => panic!("invalid edge"),
                };
                assert_eq!(drain(&mut cursor), Err(expected));
                assert_eq!(cursor.value(), None);
                assert_eq!(cursor.poll(Tick(1)), Err(expected));
                let result = cursor.finish(Tick(1));
                assert!(matches!(result, Err(e) if e == expected));
            }
        }
        for invalid in [false, true] {
            let source = if invalid {
                concat!(
                    "Content-Type:text/plain;charset*=utf-8''%zz;charset=ASCII\n",
                    "Content-Disposition:inline;filename*=utf-8''%zz;filename=x\n\n"
                )
            } else {
                concat!(
                    "Content-Type:text/plain;charset*=x-unknown''UTF-8\n",
                    "Content-Disposition:inline;filename*=x-unknown''%FF\n\n"
                )
            };
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut scratch = Scratch::new();
            let mut h = [0; 32];
            let mut c = [0; 16];
            let mut n = [0; 8];
            let mut cursor = Cursor::new(
                Entity {
                    source: source.as_bytes(),
                    base: 0,
                    source_end: SourceEnd::Eof,
                    header_limit: 1024,
                    context: mime_metadata::Context::Normal,
                },
                backing(&mut h, &mut c, &mut n),
                &mut work,
                &mut budget,
                &mut scratch,
            )
            .unwrap();
            drain(&mut cursor).unwrap();
            let view = cursor.finish(Tick(1)).unwrap().0;
            let charset = view.charset_end.unwrap();
            assert_eq!(charset.value, protocol::Value::Present);
            assert_eq!(charset.selection.invalid_extended, invalid);
            assert_eq!(charset.unsupported_qualifier, !invalid);
            assert_eq!(
                view.filename_end.origin,
                Some(mime_filename::Origin::Disposition)
            );
            assert_eq!(view.filename_end.invalid_extended, invalid);
            assert_eq!(view.filename_end.is_encoding_problem, !invalid);
            if invalid {
                assert!(matches!(
                    charset.selection.plan,
                    Some(crate::mime_parameter::Plan::Ordinary(_))
                ));
                assert_eq!(
                    charset.known_charset,
                    Some(crate::mime_charset::Charset::Ascii)
                );
                assert_eq!(view.charset, Some(b"ASCII".as_slice()));
                assert_eq!(view.filename, Some(b"x".as_slice()));
            } else {
                assert!(matches!(
                    charset.selection.plan,
                    Some(crate::mime_parameter::Plan::Extended(_))
                ));
                assert_eq!(
                    charset.known_charset,
                    Some(crate::mime_charset::Charset::Utf8)
                );
                assert_eq!(view.charset, Some(b"UTF-8".as_slice()));
                assert_eq!(view.filename, Some("�".as_bytes()));
            }
        }
    }
    #[test]
    fn folded_fields_and_head_bound() {
        assert_eq!(heads_capacity_bound(0), Some(14));
        assert_eq!(heads_capacity_bound(42), Some(56));
        assert_eq!(heads_capacity_bound(usize::MAX), None);
        let source = concat!(
            "Content-Type: TEXT\r\n / PLAIN;\r\n charset=UTf-8;\r\n name=type\r\n",
            "Content-Disposition: AtTaChMeNt;\r\n filename=\"a\r\n b\"\r\n\r\nbody"
        )
        .as_bytes();
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut h = [0; 128];
        let mut c = [0; 16];
        let mut n = [0; 16];
        let mut cursor = Cursor::new(
            Entity {
                source,
                base: 900,
                source_end: SourceEnd::Eof,
                header_limit: 1024,
                context: mime_metadata::Context::Normal,
            },
            backing(&mut h, &mut c, &mut n),
            &mut work,
            &mut budget,
            &mut scratch,
        )
        .unwrap();
        drain(&mut cursor).unwrap();
        let value = cursor.finish(Tick(1)).unwrap().0;
        assert_eq!(value.content_type, b"text/plain");
        assert_eq!(value.disposition, Some(b"attachment".as_slice()));
        assert_eq!(value.charset, Some(b"UTf-8".as_slice()));
        assert_eq!(value.filename, Some(b"a b".as_slice()));
        assert_eq!(value.body_start, 900 + source.len() as u64 - 4);
    }
}
