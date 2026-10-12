//! Complete MIME entity-header interpretation over retained immutable input.
use super::{classify, Encoding, Error, Media, Status, PARAMETER_PROBLEM};
use crate::{
    decode_work,
    header_select::SourceEnd,
    metadata,
    nfc::HeaderBudget,
    parameter::protocol,
    time::Tick,
    work::{Charge, Meter},
};

/// Complete headers or a prefix containing a definitive header boundary.
/// The enclosing owner retains source custody and any entity extent authority.
pub struct Input<'a> {
    pub source: &'a [u8],
    pub base: u64,
    pub header_limit: u64,
    pub context: metadata::Context,
    pub source_end: SourceEnd,
}
/// Passive selection, classification and validated multipart boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Value {
    pub selection: metadata::Selection,
    pub classified: classify::Value,
    pub(super) boundary: [u8; 70],
    pub(super) boundary_len: u8,
}
impl Value {
    pub fn boundary(&self) -> Option<&[u8]> {
        if self.classified.media != Media::Multipart {
            return None;
        }
        self.boundary.get(..usize::from(self.boundary_len))
    }
}
enum Phase<'a> {
    Headers(metadata::Cursor<'a>),
    Boundary(protocol::Reader<'a>),
    Complete,
}
pub(super) struct Core<'a> {
    source: &'a [u8],
    base: u64,
    phase: Phase<'a>,
    selection: Option<metadata::Selection>,
    classified: Option<classify::Value>,
    boundary: [u8; 70],
    used: usize,
}
impl<'a> Core<'a> {
    pub(super) fn new(input: Input<'a>) -> Result<Self, Error> {
        let cursor = metadata::Cursor::new(
            input.source,
            input.base,
            input.header_limit,
            input.context,
            input.source_end,
        )
        .map_err(metadata_error)?;
        Ok(Self {
            source: input.source,
            base: input.base,
            phase: Phase::Headers(cursor),
            selection: None,
            classified: None,
            boundary: [0; 70],
            used: 0,
        })
    }
    pub(super) fn value(&self) -> Option<Value> {
        if !matches!(self.phase, Phase::Complete) {
            return None;
        }
        Some(Value {
            selection: self.selection?,
            classified: self.classified?,
            boundary: self.boundary,
            boundary_len: u8::try_from(self.used).ok()?,
        })
    }
    pub(super) fn poll(
        &mut self,
        now: Tick,
        work: &mut Meter,
        budget: &mut HeaderBudget,
        credit: &mut u8,
    ) -> Result<Status, Error> {
        match &mut self.phase {
            Phase::Headers(cursor) => {
                if cursor
                    .poll_in_context(now, work, budget, credit)
                    .map_err(metadata_error)?
                    == metadata::Status::Complete
                {
                    let selection = cursor
                        .selection()
                        .map_err(metadata_error)?
                        .ok_or(Error::InvalidState)?;
                    let classified = classify::local(
                        classify::Input {
                            source: self.source,
                            base: self.base,
                            selected: selection,
                        },
                        now,
                        work,
                        budget,
                        credit,
                    )?;
                    self.selection = Some(selection);
                    self.classified = Some(classified);
                    if classified.media == Media::Multipart {
                        if classified.encoding != Encoding::Identity {
                            return Err(Error::NotParsable);
                        }
                        let raw = td_header::resident::slice(
                            self.source,
                            self.base,
                            classified.type_start..classified.type_end,
                        )
                        .ok_or(Error::InvalidRange)?;
                        self.phase = Phase::Boundary(protocol::Reader::new(
                            raw,
                            protocol::Purpose::Boundary,
                        ));
                    } else {
                        self.phase = Phase::Complete;
                    }
                }
            }
            Phase::Boundary(reader) => {
                match reader
                    .poll(now, &mut decode_work::Parsing::new(work, budget, credit))
                    .map_err(parameter_error)?
                {
                    protocol::Read::Yield => {}
                    protocol::Read::Octet(byte) => {
                        let target = self
                            .boundary
                            .get_mut(self.used)
                            .ok_or(Error::InvalidState)?;
                        work.charge(
                            now,
                            Charge {
                                output_bytes: 1,
                                ..Charge::default()
                            },
                        )
                        .map_err(Error::Work)?;
                        *target = byte;
                        self.used += 1;
                    }
                    protocol::Read::Complete(end) => {
                        if end.value != protocol::Value::Present {
                            return Err(Error::NotParsable);
                        }
                        if end.bytes != self.used {
                            return Err(Error::InvalidState);
                        }
                        if end.unsupported_qualifier || end.selection.invalid_extended {
                            self.classified
                                .as_mut()
                                .ok_or(Error::InvalidState)?
                                .diagnostics |= PARAMETER_PROBLEM;
                        }
                        self.phase = Phase::Complete;
                    }
                }
            }
            Phase::Complete => {}
        }
        Ok(if matches!(self.phase, Phase::Complete) {
            Status::Complete
        } else {
            Status::Yield
        })
    }
}
fn metadata_error(error: metadata::Error) -> Error {
    match error {
        metadata::Error::Work(stop) => Error::Work(stop),
        metadata::Error::InterpretationLimit => Error::InterpretationLimit,
        metadata::Error::Headers(crate::headers::Error::HeaderLimit) => Error::HeaderLimit,
        error => Error::Metadata(error),
    }
}
fn parameter_error(error: protocol::Error) -> Error {
    match error {
        protocol::Error::Parameter(crate::parameter::Error::Work(stop)) => Error::Work(stop),
        protocol::Error::Parameter(crate::parameter::Error::InterpretationLimit) => {
            Error::InterpretationLimit
        }
        error => Error::Parameter(error),
    }
}
/// Retains original job and aggregate header admission through interpretation.
/// It parses headers and boundary metadata; it does not traverse a MIME body.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mime::structure::entity::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mime::structure::entity::Cursor<'_, '_>>();
/// ```
pub struct Cursor<'a, 'w> {
    core: Core<'a>,
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
    credit: u8,
    failure: Option<Error>,
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub fn new(
        input: Input<'a>,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Result<Self, Error> {
        Ok(Self {
            core: Core::new(input)?,
            work,
            budget,
            credit: 0,
            failure: None,
        })
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    /// Passive observation only; use fresh consuming finish before handoff.
    pub fn value(&self) -> Option<Value> {
        if self.failure.is_some() {
            return None;
        }
        self.core.value()
    }
    /// Checks sticky refusal and both original admission domains without debit.
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self
            .budget
            .charge_local(self.work, now, 0, 0, &mut self.credit)
            .map_err(decode_work::Error::from)
            .map_err(Error::from);
        self.outcome(result)
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        self.check_deadline(now)?;
        if matches!(self.core.phase, Phase::Complete) {
            return Ok(Status::Complete);
        }
        let result = self
            .work
            .charge(
                now,
                Charge {
                    records: 1,
                    ..Charge::default()
                },
            )
            .map_err(Error::Work)
            .and_then(|()| {
                self.core
                    .poll(now, self.work, self.budget, &mut self.credit)
            });
        self.outcome(result)
    }
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<(Value, &'w mut Meter, &'w mut HeaderBudget), Error> {
        self.check_deadline(now)?;
        let value = self.core.value().ok_or(Error::InvalidState)?;
        Ok((value, self.work, self.budget))
    }
}
const _: () = assert!(std::mem::size_of::<Cursor<'_, '_>>() <= 2048);

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::{time::Deadline, work::Stop};
    fn meter(charge: Charge) -> Meter {
        Meter::new(Deadline::after(Tick(0), 100).unwrap(), charge)
    }
    fn allowance() -> Charge {
        Charge {
            io_bytes: 1_000_000,
            records: 1_000_000,
            output_bytes: 1_000_000,
            unlinks: 0,
        }
    }
    fn input(
        source: &[u8],
        base: u64,
        context: metadata::Context,
        source_end: SourceEnd,
    ) -> Input<'_> {
        Input {
            source,
            base,
            header_limit: 65536,
            context,
            source_end,
        }
    }
    fn drain(cursor: &mut Cursor<'_, '_>) -> Result<(), Error> {
        for _ in 0..10000 {
            let before = cursor.work.remaining();
            let result = cursor.poll(Tick(1));
            let after = cursor.work.remaining();
            assert!(before.io_bytes - after.io_bytes <= 352);
            assert!(before.records - after.records <= 32);
            assert!(before.output_bytes - after.output_bytes <= 1);
            if result? == Status::Complete {
                return Ok(());
            }
            assert!(cursor.value().is_none());
        }
        panic!("entity headers did not finish")
    }
    #[test]
    fn retained_prefix_and_whole_entity_have_identical_interpretation_and_costs() {
        type Case<'a> = (&'a [u8], metadata::Context, Media, Option<&'a [u8]>);
        let cases: &[Case<'_>] = &[
            (b"\r\n", metadata::Context::Normal, Media::TextPlain, None),
            (
                b"\r\n",
                metadata::Context::DigestChild,
                Media::MessageRfc822,
                None,
            ),
            (
                b"Content-Type: text/html\nContent-Transfer-Encoding: base64\n\n",
                metadata::Context::Normal,
                Media::Other,
                None,
            ),
            (
                b"Content-Type: multipart/DIGEST; boundary=abc\r\n\r\n",
                metadata::Context::Normal,
                Media::Multipart,
                Some(b"abc"),
            ),
            (
                b"Content-Type: multipart/mixed; boundary*=utf-8''ab%63\n\n",
                metadata::Context::Normal,
                Media::Multipart,
                Some(b"abc"),
            ),
        ];
        for &(headers, context, media, boundary) in cases {
            for base in [0, 20000] {
                let mut whole = headers.to_vec();
                whole.extend(vec![b'x'; 100000]);
                let mut observed = None;
                for (source, source_end) in [
                    (headers, SourceEnd::Prefix),
                    (whole.as_slice(), SourceEnd::Eof),
                ] {
                    let mut work = meter(allowance());
                    let mut budget = HeaderBudget::new();
                    let work_ptr = std::ptr::from_mut(&mut work);
                    let budget_ptr = std::ptr::from_mut(&mut budget);
                    let mut cursor = Cursor::new(
                        input(source, base, context, source_end),
                        &mut work,
                        &mut budget,
                    )
                    .unwrap();
                    drain(&mut cursor).unwrap();
                    let (value, work, budget) = cursor.finish(Tick(1)).unwrap();
                    assert_eq!(std::ptr::from_mut(work), work_ptr);
                    assert_eq!(std::ptr::from_mut(budget), budget_ptr);
                    assert_eq!(value.classified.media, media);
                    assert_eq!(value.boundary(), boundary);
                    assert_eq!(value.selection.end.body_start, base + headers.len() as u64);
                    let result = (
                        value,
                        work.remaining(),
                        budget.source_bytes_remaining(),
                        budget.steps_remaining(),
                    );
                    if let Some(expected) = observed {
                        assert_eq!(result, expected);
                    } else {
                        observed = Some(result);
                    }
                }
            }
        }
    }
    #[test]
    fn malformed_multipart_and_incomplete_headers_are_not_leaf_success() {
        for source in [
            b"Content-Type: multipart/mixed\n\n".as_slice(),
            b"Content-Type: multipart/mixed;boundary=x\nContent-Transfer-Encoding: base64\n\n",
            b"Content-Type: multipart/mixed;boundary*=utf-8''bad%3B;boundary=good\n\n",
        ] {
            let mut work = meter(allowance());
            let mut budget = HeaderBudget::new();
            let mut cursor = Cursor::new(
                input(source, 17, metadata::Context::Normal, SourceEnd::Prefix),
                &mut work,
                &mut budget,
            )
            .unwrap();
            assert_eq!(drain(&mut cursor), Err(Error::NotParsable));
            assert!(cursor.value().is_none());
            assert_eq!(cursor.poll(Tick(1)), Err(Error::NotParsable));
            assert!(matches!(cursor.finish(Tick(1)), Err(Error::NotParsable)));
        }
        let mut work = meter(allowance());
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            input(b"X:y", 17, metadata::Context::Normal, SourceEnd::Prefix),
            &mut work,
            &mut budget,
        )
        .unwrap();
        let error = Error::Metadata(metadata::Error::Truncated);
        assert_eq!(drain(&mut cursor), Err(error));
        assert!(cursor.value().is_none());
        assert_eq!(cursor.poll(Tick(1)), Err(error));
        assert!(matches!(cursor.finish(Tick(1)), Err(found) if found == error));
    }
    #[test]
    fn digest_and_parameter_diagnostics_survive_complete_interpretation() {
        for parameter in ["boundary=x;boundary*=utf-8''%zz", "boundary*=''x"] {
            let source = format!("Content-Type: multipart/DIGEST;{parameter}\n\n");
            let mut work = meter(allowance());
            let mut budget = HeaderBudget::new();
            let mut cursor = Cursor::new(
                input(
                    source.as_bytes(),
                    17,
                    metadata::Context::Normal,
                    SourceEnd::Prefix,
                ),
                &mut work,
                &mut budget,
            )
            .unwrap();
            drain(&mut cursor).unwrap();
            let value = cursor.finish(Tick(1)).unwrap().0;
            assert!(value.classified.digest);
            assert_eq!(value.classified.diagnostics, PARAMETER_PROBLEM);
            assert_eq!(value.boundary(), Some(b"x".as_slice()));
        }
        let mut work = meter(allowance());
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            input(
                b"Content-Type: multipart/mixed;boundary=\"x \"\n\n",
                0,
                metadata::Context::Normal,
                SourceEnd::Prefix,
            ),
            &mut work,
            &mut budget,
        )
        .unwrap();
        assert_eq!(drain(&mut cursor), Err(Error::NotParsable));
    }
    #[test]
    fn boundary_storage_limit_and_spelling_are_exact() {
        for length in [0, 1, 70, 71] {
            let mut source = b"Content-Type: multipart/mixed; boundary=\"".to_vec();
            source.extend(vec![b'A'; length]);
            source.extend_from_slice(b"\"\n\n");
            let mut work = meter(allowance());
            let mut budget = HeaderBudget::new();
            let mut cursor = Cursor::new(
                input(&source, 0, metadata::Context::Normal, SourceEnd::Prefix),
                &mut work,
                &mut budget,
            )
            .unwrap();
            if (1..=70).contains(&length) {
                drain(&mut cursor).unwrap();
                assert_eq!(
                    cursor.finish(Tick(1)).unwrap().0.boundary(),
                    Some(vec![b'A'; length].as_slice())
                );
            } else {
                assert_eq!(drain(&mut cursor), Err(Error::NotParsable));
            }
        }
    }
    #[test]
    fn every_job_funding_cut_retires_provisional_interpretation() {
        let source = b"Content-Type: multipart/mixed;boundary=abc\n\n";
        let mut work = meter(allowance());
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            input(source, 0, metadata::Context::Normal, SourceEnd::Prefix),
            &mut work,
            &mut budget,
        )
        .unwrap();
        drain(&mut cursor).unwrap();
        let (_, work, _) = cursor.finish(Tick(1)).unwrap();
        let used = Charge {
            io_bytes: allowance().io_bytes - work.remaining().io_bytes,
            records: allowance().records - work.remaining().records,
            output_bytes: allowance().output_bytes - work.remaining().output_bytes,
            unlinks: 0,
        };
        assert_eq!(used.output_bytes, 3);
        for (kind, total) in [
            (0, used.io_bytes),
            (1, used.records),
            (2, used.output_bytes),
        ] {
            for cut in 0..=total {
                let mut charge = allowance();
                match kind {
                    0 => charge.io_bytes = cut,
                    1 => charge.records = cut,
                    _ => charge.output_bytes = cut,
                }
                let mut work = meter(charge);
                let mut budget = HeaderBudget::new();
                let mut cursor = Cursor::new(
                    input(source, 0, metadata::Context::Normal, SourceEnd::Prefix),
                    &mut work,
                    &mut budget,
                )
                .unwrap();
                let result = drain(&mut cursor);
                if cut == total {
                    assert_eq!(result, Ok(()));
                    let before = cursor.work.remaining();
                    assert_eq!(cursor.poll(Tick(1)), Ok(Status::Complete));
                    let (_, work, _) = cursor.finish(Tick(1)).unwrap();
                    assert_eq!(work.remaining(), before);
                } else {
                    let error = result.unwrap_err();
                    assert!(matches!(error, Error::Work(_)));
                    assert!(cursor.value().is_none());
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                    assert!(matches!(cursor.finish(Tick(1)), Err(found) if found == error));
                }
            }
        }
    }
    #[test]
    fn completion_is_fresh_and_original_admission_is_not_released_early() {
        for complete in [false, true] {
            let mut work = meter(allowance());
            let mut budget = HeaderBudget::new();
            let mut cursor = Cursor::new(
                input(b"\n", 0, metadata::Context::Normal, SourceEnd::Prefix),
                &mut work,
                &mut budget,
            )
            .unwrap();
            if complete {
                drain(&mut cursor).unwrap();
                let before = cursor.work.remaining();
                assert_eq!(cursor.poll(Tick(1)), Ok(Status::Complete));
                assert_eq!(cursor.work.remaining(), before);
            }
            assert_eq!(cursor.poll(Tick(100)), Err(Error::Work(Stop::Deadline)));
            assert!(cursor.value().is_none());
            assert!(matches!(
                cursor.finish(Tick(1)),
                Err(Error::Work(Stop::Deadline))
            ));
        }
        let mut work = meter(allowance());
        let mut budget = HeaderBudget::new();
        let cursor = Cursor::new(
            input(b"\n", 0, metadata::Context::Normal, SourceEnd::Prefix),
            &mut work,
            &mut budget,
        )
        .unwrap();
        assert!(matches!(cursor.finish(Tick(1)), Err(Error::InvalidState)));
    }
}
