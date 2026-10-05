use super::{projection::Projection, Error, Form};
use crate::{
    admission::work::{Charge, Meter},
    json_string::{Frame, Progress, Status},
    nfc::HeaderBudget,
    ports::Tick,
};
mod kind;
use kind::{Event, Ids, Kind, Urls};
pub(super) struct ListMode<K>(std::marker::PhantomData<K>);
pub(super) type IdsMode = ListMode<Ids>;
pub(super) type UrlsMode = ListMode<Urls>;
impl<'a, 'w, K: Kind> Projection<'a, 'w> for ListMode<K> {
    type Source = Source<'a, 'w, K>;
    type Workspace = K::Mode;
    const FORM: Form = K::FORM;
    fn validate(
        name: &str,
        now: Tick,
        work: &mut Meter,
        budget: &mut HeaderBudget,
        mode: &mut K::Mode,
    ) -> Result<(), Error> {
        let field = K::candidate(name.len());
        budget.charge(
            work,
            now,
            field.map_or(0, |field| field.len() as u64),
            1,
            &mut 0,
        )?;
        *mode = if field.is_some_and(|field| name.eq_ignore_ascii_case(field)) {
            K::SPECIAL
        } else {
            K::DEFAULT
        };
        Ok(())
    }
    fn start(
        bytes: &'a [u8],
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
        mode: K::Mode,
    ) -> Self::Source {
        Source {
            owner: Owner::Convert(K::start(bytes, mode, work, budget)),
            mode,
            phase: Phase::First,
            array: td_json::string_array::Frame::new(),
            pending: None,
            position: 0,
            failure: None,
        }
    }
    fn finish(
        source: Self::Source,
    ) -> Result<(&'w mut Meter, &'w mut HeaderBudget, K::Mode), Error> {
        if let Some(error) = source.failure {
            return Err(error);
        }
        if !matches!(source.phase, Phase::Done) {
            return Err(Error::InvalidState);
        }
        if matches!(source.owner, Owner::Convert(_)) && !source.array.is_complete() {
            return Err(Error::InvalidState);
        }
        let (work, budget) = match source.owner {
            Owner::Convert(cursor) => K::finish(cursor).map_err(Into::<Error>::into)?,
            Owner::Budgets(work, budget) => (work, budget),
            Owner::Retired => return Err(Error::InvalidState),
        };
        Ok((work, budget, source.mode))
    }
    fn charge_output(source: &mut Self::Source, now: Tick, bytes: u64) -> Result<(), Error> {
        source.charge_output(now, bytes)
    }
    fn poll(
        source: &mut Self::Source,
        _frame: &mut Frame,
        now: Tick,
        output: &mut [u8],
    ) -> Result<Progress, Error> {
        source.poll(now, output)
    }
    fn is_encoding_problem(source: &Self::Source) -> bool {
        match &source.owner {
            Owner::Convert(cursor) => K::is_encoding_problem(cursor),
            _ => false,
        }
    }
}
enum Owner<'a, 'w, K: Kind> {
    Convert(K::Cursor<'a, 'w>),
    Budgets(&'w mut Meter, &'w mut HeaderBudget),
    Retired,
}
#[derive(Clone, Copy)]
enum Phase {
    First,
    Array,
    NullDrain,
    Done,
}
pub(super) struct Source<'a, 'w, K: Kind> {
    owner: Owner<'a, 'w, K>,
    mode: K::Mode,
    phase: Phase,
    array: td_json::string_array::Frame<Error>,
    pending: Option<Event>,
    position: usize,
    failure: Option<Error>,
}
struct ArraySource<'c, 'a, 'w, K: Kind> {
    cursor: &'c mut K::Cursor<'a, 'w>,
    pending: &'c mut Option<Event>,
}
fn contextual<K: Kind>(error: K::Failure, role: td_json::string_array::Role) -> Error {
    match role {
        td_json::string_array::Role::Array => error.into(),
        td_json::string_array::Role::String => Error::Json(error.into()),
    }
}
fn array_error(error: td_json::string_array::Error<Error>) -> Error {
    use td_json::string_array::{Error as Shared, Role};
    match error {
        Shared::Source(error) => error,
        Shared::InvalidState(Role::Array) => Error::InvalidState,
        Shared::InvalidState(Role::String) => Error::Json(crate::json_string::Error::InvalidState),
    }
}
impl<K: Kind> td_json::string_array::Source for ArraySource<'_, '_, '_, K> {
    type Context = Tick;
    type Error = Error;
    fn charge_output(
        &mut self,
        now: Tick,
        bytes: u64,
        role: td_json::string_array::Role,
    ) -> Result<(), Error> {
        K::charge_output(self.cursor, now, bytes).map_err(|error| contextual::<K>(error, role))
    }
    fn poll(&mut self, now: Tick, role: td_json::string_array::Role) -> Result<Event, Error> {
        if let Some(event) = self.pending.take() {
            return Ok(event);
        }
        K::poll(self.cursor, now).map_err(|error| contextual::<K>(error, role))
    }
}
impl<K: Kind> Source<'_, '_, K> {
    #[cfg(test)]
    pub(super) fn remaining(&self) -> Option<(Charge, u64)> {
        match &self.owner {
            Owner::Convert(cursor) => Some(K::remaining(cursor)),
            Owner::Budgets(work, budget) => Some((work.remaining(), budget.steps_remaining())),
            Owner::Retired => None,
        }
    }

    fn charge_output(&mut self, now: Tick, output_bytes: u64) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = match &mut self.owner {
            Owner::Convert(cursor) => {
                K::charge_output(cursor, now, output_bytes).map_err(Into::<Error>::into)
            }
            Owner::Budgets(work, budget) => budget
                .charge(work, now, 0, 0, &mut 0)
                .map_err(Error::from)
                .and_then(|()| {
                    work.charge(
                        now,
                        Charge {
                            output_bytes,
                            ..Charge::default()
                        },
                    )
                    .map_err(Error::Work)
                }),
            Owner::Retired => Err(Error::InvalidState),
        };
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn stage_null(&mut self, now: Tick) -> Result<(), Error> {
        self.charge_output(now, 4)?;
        self.position = 0;
        self.phase = Phase::NullDrain;
        Ok(())
    }
    fn poll(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.phase, Phase::Done) {
            return Ok(Progress {
                written: 0,
                status: Status::Complete,
            });
        }
        let result = self.step(now, output);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn step(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        if !matches!(self.phase, Phase::Array) {
            self.charge_output(now, 0)?;
        }
        if output.is_empty() && !matches!(self.phase, Phase::Array) {
            return Ok(Progress {
                written: 0,
                status: Status::NeedOutput,
            });
        }
        match self.phase {
            Phase::First => {
                let Owner::Convert(cursor) = &mut self.owner else {
                    return Err(Error::InvalidState);
                };
                match K::poll(cursor, now) {
                    Ok(Event::Yield) => {}
                    Ok(event @ (Event::Begin | Event::Complete)) => {
                        self.pending = Some(event);
                        self.phase = Phase::Array;
                    }
                    Err(error) if error == K::MALFORMED => {
                        let Owner::Convert(cursor) =
                            std::mem::replace(&mut self.owner, Owner::Retired)
                        else {
                            return Err(Error::InvalidState);
                        };
                        let (work, budget) =
                            K::finish_malformed(cursor).map_err(Into::<Error>::into)?;
                        self.owner = Owner::Budgets(work, budget);
                        self.stage_null(now)?;
                    }
                    Err(error) => return Err(error.into()),
                    Ok(Event::Scalar(_) | Event::End) => return Err(Error::InvalidState),
                }
            }
            Phase::Array => {
                let Owner::Convert(cursor) = &mut self.owner else {
                    return Err(Error::InvalidState);
                };
                let progress = self
                    .array
                    .poll(
                        &mut ArraySource::<K> {
                            cursor,
                            pending: &mut self.pending,
                        },
                        now,
                        output,
                    )
                    .map_err(array_error)?;
                if progress.status == Status::Complete {
                    self.phase = Phase::Done;
                }
                return Ok(progress);
            }
            Phase::NullDrain => {
                let count = 4usize
                    .checked_sub(self.position)
                    .ok_or(Error::InvalidState)?
                    .min(output.len());
                let end = self
                    .position
                    .checked_add(count)
                    .ok_or(Error::InvalidState)?;
                output
                    .get_mut(..count)
                    .ok_or(Error::InvalidState)?
                    .copy_from_slice(b"null".get(self.position..end).ok_or(Error::InvalidState)?);
                self.position = end;
                if end == 4 {
                    self.phase = Phase::Done;
                }
                return Ok(Progress {
                    written: count,
                    status: if matches!(self.phase, Phase::Done) {
                        Status::Complete
                    } else {
                        Status::Yield
                    },
                });
            }
            Phase::Done => return Err(Error::InvalidState),
        }
        Ok(Progress {
            written: 0,
            status: Status::Yield,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::{
        header_message_ids::{
            self as ids,
            project::{Budgeted, Status as Converted},
            Mode,
        },
        ports::Deadline,
    };
    fn work() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000,
                records: 100_000,
                output_bytes: 1000,
                ..Charge::default()
            },
        )
    }
    #[test]
    fn shared_roles_preserve_mail_error_contexts() {
        use crate::{admission::work::Stop, header_urls};
        use td_json::string_array::{Error as Shared, Role};
        let ids = ids::Error::Work(Stop::OutputBytes);
        let urls = header_urls::Error::Work(Stop::OutputBytes);
        assert_eq!(contextual::<Ids>(ids, Role::Array), Error::MessageIds(ids));
        assert_eq!(
            contextual::<Ids>(ids, Role::String),
            Error::Json(ids.into())
        );
        assert_eq!(contextual::<Urls>(urls, Role::Array), Error::URLs(urls));
        assert_eq!(
            contextual::<Urls>(urls, Role::String),
            Error::Json(urls.into())
        );
        fn charged<K: Kind>(bytes: &[u8], mode: K::Mode, expected: [Error; 2]) {
            for (role, expected) in [Role::Array, Role::String].into_iter().zip(expected) {
                let mut work = work();
                let remaining = work.remaining();
                work = Meter::new(
                    Deadline::after(Tick(0), 100).unwrap(),
                    Charge {
                        output_bytes: 0,
                        ..remaining
                    },
                );
                let mut budget = HeaderBudget::new();
                let mut cursor = K::start(bytes, mode, &mut work, &mut budget);
                let mut pending = None;
                let mut source = ArraySource::<K> {
                    cursor: &mut cursor,
                    pending: &mut pending,
                };
                assert_eq!(
                    td_json::string_array::Source::charge_output(&mut source, Tick(1), 1, role),
                    Err(expected)
                );
            }
        }
        charged::<Ids>(
            b"<a@b>",
            Mode::Strict,
            [Error::MessageIds(ids), Error::Json(ids.into())],
        );
        charged::<Urls>(
            b"<x:a>",
            header_urls::Mode::URLs,
            [Error::URLs(urls), Error::Json(urls.into())],
        );
        assert_eq!(
            array_error(Shared::InvalidState(Role::Array)),
            Error::InvalidState
        );
        assert_eq!(
            array_error(Shared::InvalidState(Role::String)),
            Error::Json(crate::json_string::Error::InvalidState)
        );
    }
    #[test]
    fn source_handoff_requires_complete_drained_value() {
        for (bytes, mode) in [
            (b"<a@b>".as_slice(), Mode::Strict),
            (b"<a@b><c@d>", Mode::Strict),
            (b"", Mode::Strict),
            (b"bad", Mode::Strict),
            (b"<a@b><c@d>", Mode::ObsoletePhrases),
        ] {
            let mut baseline_work = work();
            let mut baseline_budget = HeaderBudget::new();
            let mut source = IdsMode::start(bytes, &mut baseline_work, &mut baseline_budget, mode);
            let mut frame = Frame::new();
            let turns = (1..1000)
                .find(|_| {
                    IdsMode::poll(&mut source, &mut frame, Tick(1), &mut [0; 1])
                        .unwrap()
                        .status
                        == Status::Complete
                })
                .unwrap();
            IdsMode::finish(source).unwrap();
            for cut in 0..=turns {
                let mut work = work();
                let mut budget = HeaderBudget::new();
                let identity = (std::ptr::from_ref(&work), std::ptr::from_ref(&budget));
                let mut source = IdsMode::start(bytes, &mut work, &mut budget, mode);
                let mut frame = Frame::new();
                for _ in 0..cut {
                    IdsMode::poll(&mut source, &mut frame, Tick(1), &mut [0; 1]).unwrap();
                }
                if cut < turns {
                    assert!(matches!(IdsMode::finish(source), Err(Error::InvalidState)));
                } else {
                    let (work, budget, returned_mode) = IdsMode::finish(source).unwrap();
                    assert_eq!(
                        (std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)),
                        identity
                    );
                    assert_eq!(returned_mode, mode);
                }
            }
        }
    }

    #[test]
    fn converter_handoff_separates_malformed_from_partial_failed_and_complete() {
        let mut work = work();
        let mut budget = HeaderBudget::new();
        assert!(matches!(
            Budgeted::new(b"<a@b>", Mode::Strict, &mut work, &mut budget).finish(),
            Err(ids::Error::InvalidState)
        ));
        assert!(matches!(
            Budgeted::new(b"bad", Mode::Strict, &mut work, &mut budget).finish_malformed(),
            Err(ids::Error::InvalidState)
        ));
        for malformed in [false, true] {
            let mut cursor = Budgeted::new(b"<a@b> (bad", Mode::Strict, &mut work, &mut budget);
            for _ in 0..1000 {
                match cursor.poll(Tick(1)) {
                    Ok(Converted::Yield) => {}
                    Err(ids::Error::Malformed) => break,
                    other => panic!("unexpected outcome: {other:?}"),
                }
            }
            if malformed {
                cursor.finish_malformed().unwrap();
            } else {
                assert!(matches!(cursor.finish(), Err(ids::Error::Malformed)));
            }
        }
        let mut cursor = Budgeted::new(b"<a@b>", Mode::Strict, &mut work, &mut budget);
        let mut complete = false;
        for _ in 0..1000 {
            if cursor.poll(Tick(1)).unwrap() == Converted::Complete {
                complete = true;
                break;
            }
        }
        assert!(complete);
        assert!(matches!(
            cursor.finish_malformed(),
            Err(ids::Error::InvalidState)
        ));
        let mut cursor = Budgeted::new(b"<a@b>", Mode::Strict, &mut work, &mut budget);
        assert_eq!(
            cursor.check_deadline(Tick(100)),
            Err(ids::Error::Work(crate::admission::work::Stop::Deadline))
        );
        assert!(matches!(
            cursor.finish_malformed(),
            Err(ids::Error::InvalidState)
        ));
    }
    #[test]
    fn url_handoffs_require_complete_or_whole_field_malformed_state() {
        use crate::header_urls::{
            Budgeted as UrlBudgeted, Error as UrlError, Mode as UrlMode, Status as UrlStatus,
        };
        let mut work = work();
        let mut budget = HeaderBudget::new();
        assert!(matches!(
            UrlBudgeted::new(b"<x:a>", UrlMode::URLs, &mut work, &mut budget).finish(),
            Err(UrlError::InvalidState)
        ));
        assert!(matches!(
            UrlBudgeted::new(b"bad", UrlMode::URLs, &mut work, &mut budget).finish_malformed(),
            Err(UrlError::InvalidState)
        ));
        for malformed in [false, true] {
            let mut cursor = UrlBudgeted::new(b"<x:a> (bad", UrlMode::URLs, &mut work, &mut budget);
            for _ in 0..1000 {
                match cursor.poll(Tick(1)) {
                    Ok(UrlStatus::Yield) => {}
                    Err(UrlError::Malformed) => break,
                    other => panic!("unexpected outcome: {other:?}"),
                }
            }
            if malformed {
                cursor.finish_malformed().unwrap();
            } else {
                assert!(matches!(cursor.finish(), Err(UrlError::Malformed)));
            }
        }
        let mut cursor = UrlBudgeted::new(b"<x:a>", UrlMode::URLs, &mut work, &mut budget);
        let mut complete = false;
        for _ in 0..1000 {
            if cursor.poll(Tick(1)).unwrap() == UrlStatus::Complete {
                complete = true;
                break;
            }
        }
        assert!(complete);
        assert!(matches!(
            cursor.finish_malformed(),
            Err(UrlError::InvalidState)
        ));
        let mut source = UrlsMode::start(b"bad", &mut work, &mut budget, UrlMode::URLs);
        let mut frame = Frame::new();
        for _ in 0..1000 {
            UrlsMode::poll(&mut source, &mut frame, Tick(1), &mut [0; 1]).unwrap();
            if matches!(source.phase, Phase::NullDrain) {
                break;
            }
        }
        assert!(matches!(source.owner, Owner::Budgets(_, _)));
        assert_eq!(
            UrlsMode::poll(&mut source, &mut frame, Tick(1), &mut [0; 1])
                .unwrap()
                .written,
            1
        );
        assert!(matches!(UrlsMode::finish(source), Err(Error::InvalidState)));
        let mut cursor = UrlBudgeted::new(b"<x:a>", UrlMode::URLs, &mut work, &mut budget);
        assert_eq!(
            cursor.check_deadline(Tick(100)),
            Err(UrlError::Work(crate::admission::work::Stop::Deadline))
        );
        assert!(matches!(
            cursor.finish_malformed(),
            Err(UrlError::InvalidState)
        ));
    }
}
