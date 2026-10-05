//! Retain selected CID/language JSON in separate caller-reserved windows.
use crate::{
    admission::work::Meter,
    mime_content_id::json as cid,
    mime_language::json as language,
    nfc::{self, HeaderBudget},
    ports::Tick,
};
#[derive(Clone, Copy)]
pub struct Values<'a> {
    pub content_id: Option<&'a [u8]>,
    pub content_language: Option<&'a [u8]>,
}
pub struct Backing<'w> {
    pub content_id: &'w mut [u8],
    pub content_language: &'w mut [u8],
}
/// Conservative serialized CID capacity from the complete raw value length.
/// Sizing grants no source, validity or output allowance.
pub const fn content_id_capacity_bound(raw_bytes: usize) -> Option<usize> {
    td_json::string::capacity_bound(raw_bytes)
}
/// Conservative serialized language-array capacity from raw value length.
pub const fn content_language_capacity_bound(raw_bytes: usize) -> Option<usize> {
    match raw_bytes.checked_mul(2) {
        Some(bytes) => bytes.checked_add(3),
        None => None,
    }
}
/// Complete passive JSON fragments; enclosing response publication is separate.
pub struct Retained<'w> {
    pub content_id: Option<&'w [u8]>,
    pub content_id_end: Option<cid::End>,
    pub content_language: Option<&'w [u8]>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    ContentId(cid::Error),
    ContentLanguage(language::Error),
    Admission(nfc::Error),
    OutputCapacity,
    InvalidState,
}
fn window_error(error: td_json::retain::Error) -> Error {
    match error {
        td_json::retain::Error::Capacity => Error::OutputCapacity,
        td_json::retain::Error::InvalidState => Error::InvalidState,
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ContentId(error) => write!(f, "retained CID JSON: {error}"),
            Self::ContentLanguage(error) => write!(f, "retained language JSON: {error}"),
            Self::Admission(error) => write!(f, "retained label admission: {error}"),
            Self::OutputCapacity => f.write_str("retained label JSON capacity"),
            Self::InvalidState => f.write_str("invalid retained label JSON state"),
        }
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Complete,
}
#[derive(Clone, Copy)]
enum Phase {
    StartId,
    Id,
    StartLanguage,
    Language,
    Complete,
}
// Original owners are held by exactly one child at a time.
#[allow(clippy::large_enum_variant)]
enum Owner<'a, 'w> {
    Budgets(&'w mut Meter, &'w mut HeaderBudget),
    Id(cid::Cursor<'a, 'w>),
    Language(language::Cursor<'a, 'w>),
    Retired,
}
/// Caller supplies authorized complete selected values and reserved backing.
/// Missing values remain absent; selected syntax failures retire the whole pair.
/// Backing bytes remain provisional through healthy pair completion and finish.
/// No field discovery, null mapping, allocation or source/publication grant.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_label_fields::json::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_label_fields::json::Cursor<'_, '_>>();
/// ```
pub struct Cursor<'a, 'w> {
    values: Values<'a>,
    id_window: td_json::retain::Window<'w>,
    language_window: td_json::retain::Window<'w>,
    owner: Owner<'a, 'w>,
    phase: Phase,
    id_end: Option<cid::End>,
    language_complete: bool,
    failure: Option<Error>,
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub const fn new(
        values: Values<'a>,
        backing: Backing<'w>,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Self {
        Self {
            values,
            id_window: td_json::retain::Window::new(backing.content_id),
            language_window: td_json::retain::Window::new(backing.content_language),
            owner: Owner::Budgets(work, budget),
            phase: Phase::StartId,
            id_end: None,
            language_complete: false,
            failure: None,
        }
    }
    pub fn is_complete(&self) -> bool {
        self.failure.is_none()
            && matches!(self.phase, Phase::Complete)
            && (self.values.content_id.is_none() || self.id_end.is_some())
            && (self.values.content_language.is_none() || self.language_complete)
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = match &mut self.owner {
            Owner::Budgets(work, budget) => budget
                .charge(work, now, 0, 0, &mut 0)
                .map_err(Error::Admission),
            Owner::Id(cursor) => cursor.check_deadline(now).map_err(Error::ContentId),
            Owner::Language(cursor) => cursor.check_deadline(now).map_err(Error::ContentLanguage),
            Owner::Retired => Err(Error::InvalidState),
        };
        self.outcome(result)
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.is_complete() {
            return Ok(Status::Complete);
        }
        self.check_deadline(now)?;
        let result = self.step(now);
        self.outcome(result)
    }
    fn budgets(&mut self) -> Result<(&'w mut Meter, &'w mut HeaderBudget), Error> {
        match std::mem::replace(&mut self.owner, Owner::Retired) {
            Owner::Budgets(work, budget) => Ok((work, budget)),
            _ => Err(Error::InvalidState),
        }
    }
    fn step(&mut self, now: Tick) -> Result<Status, Error> {
        match self.phase {
            Phase::StartId => {
                if let Some(source) = self.values.content_id {
                    let (work, budget) = self.budgets()?;
                    self.owner = Owner::Id(cid::Cursor::new(source, work, budget));
                    self.phase = Phase::Id;
                } else {
                    self.phase = Phase::StartLanguage;
                }
                Ok(Status::Yield)
            }
            Phase::Id => {
                let Owner::Id(cursor) = &mut self.owner else {
                    return Err(Error::InvalidState);
                };
                let output = self.id_window.tail().map_err(window_error)?;
                let progress = cursor.poll(now, output).map_err(Error::ContentId)?;
                self.id_window
                    .advance(progress.written)
                    .map_err(window_error)?;
                if progress.status == cid::Status::Complete {
                    let Owner::Id(cursor) = std::mem::replace(&mut self.owner, Owner::Retired)
                    else {
                        return Err(Error::InvalidState);
                    };
                    let (work, budget, end) = cursor.finish(now).map_err(Error::ContentId)?;
                    self.id_end = Some(end);
                    self.owner = Owner::Budgets(work, budget);
                    self.phase = Phase::StartLanguage;
                }
                Ok(Status::Yield)
            }
            Phase::StartLanguage => {
                if let Some(source) = self.values.content_language {
                    let (work, budget) = self.budgets()?;
                    self.owner = Owner::Language(language::Cursor::new(source, work, budget));
                    self.phase = Phase::Language;
                    Ok(Status::Yield)
                } else {
                    self.phase = Phase::Complete;
                    Ok(Status::Complete)
                }
            }
            Phase::Language => {
                let Owner::Language(cursor) = &mut self.owner else {
                    return Err(Error::InvalidState);
                };
                let output = self.language_window.tail().map_err(window_error)?;
                let progress = cursor.poll(now, output).map_err(Error::ContentLanguage)?;
                self.language_window
                    .advance(progress.written)
                    .map_err(window_error)?;
                if progress.status == language::Status::Complete {
                    let Owner::Language(cursor) =
                        std::mem::replace(&mut self.owner, Owner::Retired)
                    else {
                        return Err(Error::InvalidState);
                    };
                    let (work, budget) = cursor.finish(now).map_err(Error::ContentLanguage)?;
                    self.language_complete = true;
                    self.owner = Owner::Budgets(work, budget);
                    self.phase = Phase::Complete;
                    return Ok(Status::Complete);
                }
                Ok(Status::Yield)
            }
            Phase::Complete => Err(Error::InvalidState),
        }
    }
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<(Retained<'w>, &'w mut Meter, &'w mut HeaderBudget), Error> {
        self.check_deadline(now)?;
        if !self.is_complete() {
            return Err(Error::InvalidState);
        }
        let (work, budget) = self.budgets()?;
        let content_id = match self.values.content_id {
            Some(_) if self.id_end.is_some() => {
                Some(self.id_window.into_slice().map_err(window_error)?)
            }
            Some(_) => return Err(Error::InvalidState),
            None => None,
        };
        let content_language = match self.values.content_language {
            Some(_) if self.language_complete => {
                Some(self.language_window.into_slice().map_err(window_error)?)
            }
            Some(_) => return Err(Error::InvalidState),
            None => None,
        };
        Ok((
            Retained {
                content_id,
                content_id_end: self.id_end,
                content_language,
            },
            work,
            budget,
        ))
    }
}
const _: () =
    assert!(std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 1024);

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{
        admission::work::{Charge, Stop},
        ports::Deadline,
    };
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
    fn drain(cursor: &mut Cursor<'_, '_>) -> Result<usize, Error> {
        for turn in 1..200_000 {
            let before = match &cursor.owner {
                Owner::Budgets(work, budget) => (work.remaining(), budget.steps_remaining()),
                Owner::Id(_) | Owner::Language(_) => {
                    // Costs are independently pinned by each selected serializer.
                    // Child owners remain inaccessible to this enclosing collector.
                    if cursor.poll(Tick(1))? == Status::Complete {
                        return Ok(turn);
                    }
                    assert!(!cursor.is_complete());
                    continue;
                }
                Owner::Retired => panic!("retired owner"),
            };
            let status = cursor.poll(Tick(1))?;
            if let Owner::Budgets(work, budget) = &cursor.owner {
                assert_eq!((work.remaining(), budget.steps_remaining()), before);
            }
            if status == Status::Complete {
                return Ok(turn);
            }
            assert!(!cursor.is_complete());
        }
        panic!("retained label pair did not complete")
    }
    fn values() -> Values<'static> {
        Values {
            content_id: Some(b"<A@B>"),
            content_language: Some(b"en, en"),
        }
    }
    #[test]
    fn exact_windows_presence_diagnostics_and_original_owner_reuse() {
        assert_eq!(content_id_capacity_bound(0), Some(2));
        assert_eq!(content_id_capacity_bound(1), Some(8));
        assert_eq!(content_id_capacity_bound(usize::MAX), None);
        assert_eq!(content_language_capacity_bound(0), Some(3));
        assert_eq!(content_language_capacity_bound(1), Some(5));
        assert_eq!(content_language_capacity_bound(usize::MAX), None);

        for (id, lang, expected_id, expected_lang, problem) in [
            (None, None, None, None, false),
            (Some("<A@B>"), None, Some("\"A@B\""), None, false),
            (None, Some("en, en"), None, Some("[\"en\",\"en\"]"), false),
            (
                Some("<\u{fdd0}@B>"),
                Some("EN-us, en-US"),
                Some("\"�@B\""),
                Some("[\"EN-us\",\"en-US\"]"),
                true,
            ),
        ] {
            if let Some(raw) = id {
                assert!(
                    content_id_capacity_bound(raw.len()).unwrap() >= expected_id.unwrap().len()
                );
            }
            if let Some(raw) = lang {
                assert!(
                    content_language_capacity_bound(raw.len()).unwrap()
                        >= expected_lang.unwrap().len()
                );
            }
            for excess in 0..=2 {
                let mut work = meter();
                let initial_output = work.remaining().output_bytes;
                let mut budget = HeaderBudget::new();
                let identity = (std::ptr::from_ref(&work), std::ptr::from_ref(&budget));
                let mut id_output = [0xa5; 64];
                let mut lang_output = [0xa5; 64];
                let mut cursor = Cursor::new(
                    Values {
                        content_id: id.map(str::as_bytes),
                        content_language: lang.map(str::as_bytes),
                    },
                    Backing {
                        content_id: &mut id_output[..expected_id.map_or(0, str::len) + excess],
                        content_language: &mut lang_output
                            [..expected_lang.map_or(0, str::len) + excess],
                    },
                    &mut work,
                    &mut budget,
                );
                assert!(!cursor.is_complete());
                assert!(drain(&mut cursor).is_ok());
                assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                let (retained, work, budget) = cursor.finish(Tick(1)).unwrap();
                assert_eq!(retained.content_id, expected_id.map(str::as_bytes));
                assert_eq!(retained.content_language, expected_lang.map(str::as_bytes));
                let id_bytes = expected_id.map_or(0, str::len);
                let conversion = expected_id.map_or(0, |value| 2 * (value.len() - 2));
                let language_bytes = expected_lang.map_or(0, str::len);
                assert_eq!(
                    initial_output - work.remaining().output_bytes,
                    (id_bytes + conversion + language_bytes) as u64
                );
                assert_eq!(
                    retained.content_id_end.map(|end| end.is_encoding_problem),
                    id.map(|_| problem)
                );
                assert_eq!(
                    (std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)),
                    identity
                );
                let mut next = crate::mime_language::Cursor::new(b"fr", work, budget);
                while next.poll(Tick(1)).unwrap() != crate::mime_language::Status::Complete {}
                let (work, budget) = next.finish(Tick(1)).unwrap();
                assert_eq!(
                    (std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)),
                    identity
                );
                assert!(id_output
                    .get(expected_id.map_or(0, str::len)..)
                    .unwrap()
                    .iter()
                    .all(|byte| *byte == 0xa5));
                assert!(lang_output
                    .get(expected_lang.map_or(0, str::len)..)
                    .unwrap()
                    .iter()
                    .all(|byte| *byte == 0xa5));
            }
        }
    }
    #[test]
    fn every_capacity_cut_and_bad_language_tail_retire_the_pair() {
        for kind in 0..2 {
            for capacity in 0..if kind == 0 { 5 } else { 11 } {
                let mut work = meter();
                let mut budget = HeaderBudget::new();
                let mut id = [0; 5];
                let mut lang = [0; 11];
                let mut cursor = Cursor::new(
                    values(),
                    Backing {
                        content_id: &mut id[..if kind == 0 { capacity } else { 5 }],
                        content_language: &mut lang[..if kind == 1 { capacity } else { 11 }],
                    },
                    &mut work,
                    &mut budget,
                );
                assert_eq!(drain(&mut cursor), Err(Error::OutputCapacity));
                if kind == 1 {
                    assert!(cursor.id_end.is_some());
                    assert_eq!(cursor.id_window.provisional(), Some(b"\"A@B\"".as_slice()));
                }
                assert!(!cursor.is_complete());
                assert_eq!(cursor.poll(Tick(1)), Err(Error::OutputCapacity));
                assert_eq!(cursor.check_deadline(Tick(100)), Err(Error::OutputCapacity));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::OutputCapacity));
                assert_eq!(work.stopped(), None);
            }
        }
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut id = [0; 5];
        let mut lang = [0; 64];
        let mut cursor = Cursor::new(
            Values {
                content_id: Some(b"<A@B>"),
                content_language: Some(b"en,"),
            },
            Backing {
                content_id: &mut id,
                content_language: &mut lang,
            },
            &mut work,
            &mut budget,
        );
        let error = Error::ContentLanguage(language::Error::Source(
            crate::mime_language::Error::Malformed,
        ));
        assert_eq!(drain(&mut cursor), Err(error));
        assert!(cursor.id_end.is_some());
        assert!(!cursor.is_complete());
        assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
    }
    #[test]
    fn original_resource_cuts_and_exact_grants() {
        let mut work = meter();
        let initial = work.remaining();
        let mut budget = HeaderBudget::new();
        let mut id = [0; 5];
        let mut lang = [0; 11];
        let mut cursor = Cursor::new(
            values(),
            Backing {
                content_id: &mut id,
                content_language: &mut lang,
            },
            &mut work,
            &mut budget,
        );
        assert!(drain(&mut cursor).is_ok());
        cursor.finish(Tick(1)).unwrap();
        let used = [
            HeaderBudget::new().source_bytes_remaining() - budget.source_bytes_remaining(),
            HeaderBudget::new().steps_remaining() - budget.steps_remaining(),
            initial.io_bytes - work.remaining().io_bytes,
            initial.records - work.remaining().records,
            initial.output_bytes - work.remaining().output_bytes,
        ];
        assert!(used.iter().all(|cost| *cost > 0));
        for (kind, cost) in used.into_iter().enumerate() {
            for cap in 0..cost {
                let mut work = meter();
                let mut budget = HeaderBudget::new();
                if kind < 2 {
                    let bytes = if kind == 0 {
                        budget.source_bytes_remaining() - cap
                    } else {
                        0
                    };
                    let steps = if kind == 1 {
                        budget.steps_remaining() - cap
                    } else {
                        0
                    };
                    budget
                        .charge(&mut meter(), Tick(1), bytes, steps, &mut 0)
                        .unwrap();
                } else {
                    let mut grants = work.remaining();
                    match kind {
                        2 => grants.io_bytes = cap,
                        3 => grants.records = cap,
                        4 => grants.output_bytes = cap,
                        _ => panic!("bad cut"),
                    }
                    work = Meter::new(Deadline::after(Tick(0), 100).unwrap(), grants);
                }
                let mut id = [0; 5];
                let mut lang = [0; 11];
                let mut cursor = Cursor::new(
                    values(),
                    Backing {
                        content_id: &mut id,
                        content_language: &mut lang,
                    },
                    &mut work,
                    &mut budget,
                );
                let error = drain(&mut cursor).unwrap_err();
                assert!(!cursor.is_complete());
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                if kind < 2 {
                    let mut next = crate::mime_language::Cursor::new(b"fr", &mut work, &mut budget);
                    assert_eq!(
                        next.poll(Tick(1)),
                        Err(crate::mime_language::Error::InterpretationLimit)
                    );
                } else {
                    assert_eq!(
                        work.stopped(),
                        Some(match kind {
                            2 => Stop::IoBytes,
                            3 => Stop::Records,
                            4 => Stop::OutputBytes,
                            _ => panic!("bad cut"),
                        })
                    );
                }
            }
        }
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: used[2],
                records: used[3],
                output_bytes: used[4],
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let (bytes, steps) = (
            budget.source_bytes_remaining() - used[0],
            budget.steps_remaining() - used[1],
        );
        budget
            .charge(&mut meter(), Tick(1), bytes, steps, &mut 0)
            .unwrap();
        let mut id = [0; 5];
        let mut lang = [0; 11];
        let mut cursor = Cursor::new(
            values(),
            Backing {
                content_id: &mut id,
                content_language: &mut lang,
            },
            &mut work,
            &mut budget,
        );
        assert!(drain(&mut cursor).is_ok());
        cursor.finish(Tick(1)).unwrap();
        assert_eq!(work.remaining(), Charge::default());
        assert_eq!(
            (budget.source_bytes_remaining(), budget.steps_remaining()),
            (0, 0)
        );
    }
    #[test]
    fn fresh_deadline_and_premature_finish_at_every_pair_cut() {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut id = [0; 5];
        let mut lang = [0; 11];
        let mut cursor = Cursor::new(
            values(),
            Backing {
                content_id: &mut id,
                content_language: &mut lang,
            },
            &mut work,
            &mut budget,
        );
        let turns = drain(&mut cursor).unwrap();
        cursor.finish(Tick(1)).unwrap();
        for cut in 0..=turns {
            for trial in 0..3 {
                let mut work = meter();
                let mut budget = HeaderBudget::new();
                let mut id = [0; 5];
                let mut lang = [0; 11];
                let mut cursor = Cursor::new(
                    values(),
                    Backing {
                        content_id: &mut id,
                        content_language: &mut lang,
                    },
                    &mut work,
                    &mut budget,
                );
                for _ in 0..cut {
                    cursor.poll(Tick(1)).unwrap();
                }
                let expected = match &cursor.owner {
                    Owner::Budgets(..) => Error::Admission(nfc::Error::Work(Stop::Deadline)),
                    Owner::Id(_) => Error::ContentId(cid::Error::Source(
                        crate::mime_content_id::Error::Work(Stop::Deadline),
                    )),
                    Owner::Language(_) => Error::ContentLanguage(language::Error::Source(
                        crate::mime_language::Error::Work(Stop::Deadline),
                    )),
                    Owner::Retired => panic!("retired healthy cursor"),
                };
                if trial == 0 {
                    let error = cursor.check_deadline(Tick(100)).unwrap_err();
                    assert_eq!(error, expected);
                    assert!(!cursor.is_complete());
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                    assert_eq!(work.stopped(), Some(Stop::Deadline));
                } else if trial == 1 {
                    assert_eq!(cursor.finish(Tick(100)).err(), Some(expected));
                    assert_eq!(work.stopped(), Some(Stop::Deadline));
                } else if cut < turns {
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::InvalidState));
                    assert_eq!(work.stopped(), None);
                } else {
                    assert!(cursor.finish(Tick(1)).is_ok());
                }
            }
        }
    }
    #[derive(Debug, Eq, PartialEq)]
    struct Costs {
        work: Charge,
        bytes: u64,
        steps: u64,
    }
    fn costs(work: &Meter, budget: &HeaderBudget) -> Costs {
        Costs {
            work: work.remaining(),
            bytes: budget.source_bytes_remaining(),
            steps: budget.steps_remaining(),
        }
    }
    fn standalone_turns() -> (usize, usize) {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut output = [0; 6];
        let mut cursor = cid::Cursor::new(b"<A@B>", &mut work, &mut budget);
        let mut id_turns = 0;
        for turn in 1..1000 {
            if cursor.poll(Tick(1), &mut output).unwrap().status == cid::Status::Complete {
                id_turns = turn;
                break;
            }
        }
        assert!(id_turns > 0);
        let (work, budget, _) = cursor.finish(Tick(1)).unwrap();
        let mut cursor = language::Cursor::new(b"en, en", work, budget);
        let mut language_turns = 0;
        for turn in 1..1000 {
            if cursor.poll(Tick(1), &mut output).unwrap().status == language::Status::Complete {
                language_turns = turn;
                break;
            }
        }
        assert!(language_turns > 0);
        cursor.finish(Tick(1)).unwrap();
        (id_turns, language_turns)
    }
    fn standalone_prefix(cut: usize, id_turns: usize) -> Costs {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut output = [0; 6];
        {
            let mut cursor = cid::Cursor::new(b"<A@B>", &mut work, &mut budget);
            for _ in 0..cut.saturating_sub(1).min(id_turns) {
                cursor.poll(Tick(1), &mut output).unwrap();
            }
            // Premature finish freshly admits but adds no charge; releasing the
            // child lets the oracle inspect the original owners without a hook.
            let _ = cursor.finish(Tick(1));
        }
        let language_polls = cut.saturating_sub(id_turns + 2);
        if language_polls > 0 {
            let mut cursor = language::Cursor::new(b"en, en", &mut work, &mut budget);
            for _ in 0..language_polls {
                cursor.poll(Tick(1), &mut output).unwrap();
            }
            let _ = cursor.finish(Tick(1));
        }
        costs(&work, &budget)
    }
    #[test]
    fn every_turn_matches_standalone_child_cost_and_completion() {
        let (id_turns, language_turns) = standalone_turns();
        let turns = 2 + id_turns + language_turns;
        for cut in 0..=turns {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut id = [0; 5];
            let mut language = [0; 11];
            let mut cursor = Cursor::new(
                values(),
                Backing {
                    content_id: &mut id,
                    content_language: &mut language,
                },
                &mut work,
                &mut budget,
            );
            for _ in 0..cut {
                cursor.poll(Tick(1)).unwrap();
            }
            assert_eq!(cursor.is_complete(), cut == turns, "completion cut {cut}");
            let result = cursor.finish(Tick(1));
            assert_eq!(result.is_ok(), cut == turns);
            let _ = result;
            assert_eq!(
                costs(&work, &budget),
                standalone_prefix(cut, id_turns),
                "cost cut {cut}"
            );
        }
    }
}
