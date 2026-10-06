//! Complete parameter-name classification with original mail admission.
use crate::{
    decode_work::{self, Lexical, Work},
    nfc::HeaderBudget,
    time::Tick,
    work::{Meter, Stop},
};
pub use td_header::mime_attribute::{Extent, Form, Name, Status};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Work(Stop),
    InterpretationLimit,
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Work(error) => write!(f, "MIME attribute work: {error}"),
            Self::InterpretationLimit => f.write_str("MIME attribute interpretation limit"),
            Self::InvalidState => f.write_str("invalid MIME attribute state"),
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
impl From<td_header::mime_attribute::Error<decode_work::Error>> for Error {
    fn from(error: td_header::mime_attribute::Error<decode_work::Error>) -> Self {
        match error {
            td_header::mime_attribute::Error::Work(error) => Self::from(error),
            td_header::mime_attribute::Error::InvalidState => Self::InvalidState,
        }
    }
}
/// The caller authorizes a complete parameter-name slice, not a field prefix.
/// Classification does not select or decode a parameter value.
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {}
/// cloned::<td_mime::attribute::Cursor<'_>>();
/// ```
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {}
/// copied::<td_mime::attribute::Cursor<'_>>();
/// ```
pub struct Cursor<'a> {
    inner: td_header::mime_attribute::Cursor<'a, decode_work::Error>,
}
impl<'a> Cursor<'a> {
    pub(crate) fn checkpoint(
        &self,
    ) -> Result<td_header::mime_attribute::Checkpoint<'a, decode_work::Error>, Error> {
        self.inner.checkpoint().map_err(Error::from)
    }
    pub(crate) fn resume(
        checkpoint: td_header::mime_attribute::Checkpoint<'a, decode_work::Error>,
    ) -> Self {
        Self {
            inner: checkpoint.resume(),
        }
    }

    #[must_use]
    pub const fn new(name: &'a [u8]) -> Self {
        Self {
            inner: td_header::mime_attribute::Cursor::new(name),
        }
    }
    /// Fresh plain-job admission; refusal retires this same cursor.
    pub fn check_deadline(&mut self, now: Tick, work: &mut Meter) -> Result<(), Error> {
        self.inner
            .check_work(&mut Lexical::new(work, now))
            .map_err(Error::from)
    }
    pub fn poll(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        self.poll_with_work(now, work)
    }
    pub(crate) fn poll_with_work(
        &mut self,
        now: Tick,
        work: &mut impl Work,
    ) -> Result<Status, Error> {
        self.inner
            .poll(&mut Lexical::new(work, now))
            .map_err(Error::from)
    }
}
/// Same original job and aggregate work through name completion.
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {}
/// cloned::<td_mime::attribute::Budgeted<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {}
/// copied::<td_mime::attribute::Budgeted<'_, '_>>();
/// ```
pub struct Budgeted<'a, 'w> {
    cursor: Cursor<'a>,
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
    credit: u8,
}
impl<'a, 'w> Budgeted<'a, 'w> {
    #[must_use]
    pub fn new(name: &'a [u8], work: &'w mut Meter, budget: &'w mut HeaderBudget) -> Self {
        Self {
            cursor: Cursor::new(name),
            work,
            budget,
            credit: 0,
        }
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.cursor
            .inner
            .check_work(&mut decode_work::Admission::new(
                now,
                self.work,
                self.budget,
                &mut self.credit,
            ))
            .map_err(Error::from)
    }
    /// Cached completion is inert; use check_deadline for fresh admission.
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        self.cursor.poll_with_work(
            now,
            &mut decode_work::Parsing::new(self.work, self.budget, &mut self.credit),
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{time::Deadline, work::Charge};
    fn work() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000_000,
                records: 10_000_000,
                ..Charge::default()
            },
        )
    }
    #[test]
    fn plain_facade_preserves_name_case_form_and_exact_costs() {
        for (source, base, form) in [
            (
                b"FiLeNaMe*0*".as_slice(),
                b"FiLeNaMe".as_slice(),
                Form::Section {
                    index: 0,
                    encoded: true,
                },
            ),
            (b"filename*01", b"filename", Form::Malformed),
            (b"filename%", b"filename%", Form::Ordinary),
        ] {
            let mut cursor = Cursor::new(source);
            assert!(std::mem::size_of_val(&cursor) <= 128);
            let mut admission = work();
            let before = admission.remaining();
            cursor.check_deadline(Tick(1), &mut admission).unwrap();
            assert_eq!(admission.remaining(), before);
            loop {
                if let Status::Complete(name) = cursor.poll(Tick(1), &mut admission).unwrap() {
                    assert_eq!(name.form, form);
                    assert_eq!(
                        name.base,
                        Extent {
                            start: 0,
                            end: base.len()
                        }
                    );
                    assert_eq!(source.get(name.base.start..name.base.end), Some(base));
                    let length = source.len() as u64;
                    assert_eq!(before.io_bytes - admission.remaining().io_bytes, length);
                    assert_eq!(before.records - admission.remaining().records, length + 1);
                    let before = admission.remaining();
                    cursor.check_deadline(Tick(1), &mut admission).unwrap();
                    assert_eq!(admission.remaining(), before);
                    assert_eq!(
                        cursor.poll(Tick(100), &mut admission),
                        Ok(Status::Complete(name))
                    );
                    assert_eq!(admission.remaining(), before);
                    assert_eq!(
                        cursor.check_deadline(Tick(100), &mut admission),
                        Err(Error::Work(Stop::Deadline))
                    );
                    let mut fresh = work();
                    assert_eq!(
                        cursor.poll(Tick(1), &mut fresh),
                        Err(Error::Work(Stop::Deadline))
                    );
                    break;
                }
            }
        }
    }
    #[test]
    fn budgeted_long_name_turns_and_cached_retirement() {
        let source = format!("{}*13*", "a".repeat(4096));
        let mut admission = work();
        let mut budget = HeaderBudget::new();
        let before = (budget.source_bytes_remaining(), budget.steps_remaining());
        let mut cursor = Budgeted::new(source.as_bytes(), &mut admission, &mut budget);
        assert!(std::mem::size_of_val(&cursor) <= 160);
        for _ in 0..1000 {
            let turn = (
                cursor.work.remaining(),
                cursor.budget.source_bytes_remaining(),
                cursor.budget.steps_remaining(),
            );
            let credit = cursor.credit;
            cursor.check_deadline(Tick(1)).unwrap();
            assert_eq!(cursor.credit, credit);
            assert_eq!(
                (
                    cursor.work.remaining(),
                    cursor.budget.source_bytes_remaining(),
                    cursor.budget.steps_remaining()
                ),
                turn
            );
            let result = cursor.poll(Tick(1)).unwrap();
            assert!(turn.0.io_bytes - cursor.work.remaining().io_bytes <= 32);
            assert!(turn.0.records - cursor.work.remaining().records <= 4);
            assert!(turn.1 - cursor.budget.source_bytes_remaining() <= 32);
            assert!(turn.2 - cursor.budget.steps_remaining() <= 64);
            if let Status::Complete(name) = result {
                assert_eq!(
                    name.base,
                    Extent {
                        start: 0,
                        end: 4096
                    }
                );
                assert_eq!(
                    name.form,
                    Form::Section {
                        index: 13,
                        encoded: true
                    }
                );
                assert_eq!(
                    before.0 - cursor.budget.source_bytes_remaining(),
                    source.len() as u64
                );
                assert_eq!(
                    before.1 - cursor.budget.steps_remaining(),
                    2 * source.len() as u64 + 1
                );
                let complete = (
                    cursor.work.remaining(),
                    cursor.budget.source_bytes_remaining(),
                    cursor.budget.steps_remaining(),
                    cursor.credit,
                );
                cursor.check_deadline(Tick(1)).unwrap();
                assert_eq!(
                    (
                        cursor.work.remaining(),
                        cursor.budget.source_bytes_remaining(),
                        cursor.budget.steps_remaining(),
                        cursor.credit
                    ),
                    complete
                );
                assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete(name)));
                assert_eq!(
                    cursor.check_deadline(Tick(100)),
                    Err(Error::Work(Stop::Deadline))
                );
                assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
                return;
            }
        }
        panic!("not complete");
    }
    #[test]
    fn all_partial_aggregate_cuts_retire_name_classification() {
        let source = b"filename*13*";
        for bytes in [true, false] {
            let amount = if bytes {
                source.len() as u64
            } else {
                2 * source.len() as u64 + 1
            };
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
                    .charge_local(&mut admission, Tick(1), visits, steps, &mut credit)
                    .unwrap();
                let mut cursor = Budgeted::new(source, &mut admission, &mut budget);
                loop {
                    match cursor.poll(Tick(1)) {
                        Ok(Status::Yield) => {}
                        Ok(Status::Complete(_)) => panic!("partial aggregate admitted"),
                        Err(error) => {
                            assert_eq!(error, Error::InterpretationLimit);
                            assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                            assert_eq!(cursor.poll(Tick(1)), Err(error));
                            break;
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn final_eof_deadline_and_replacement_meter_do_not_revive() {
        let source = b"abcdefghijklmnopqrstuvwxyzabcdef";
        let mut cursor = Cursor::new(source);
        let mut admission = work();
        assert_eq!(cursor.poll(Tick(1), &mut admission), Ok(Status::Yield));
        assert_eq!(
            cursor.poll(Tick(100), &mut admission),
            Err(Error::Work(Stop::Deadline))
        );
        let mut fresh = work();
        let before = fresh.remaining();
        assert_eq!(
            cursor.poll(Tick(1), &mut fresh),
            Err(Error::Work(Stop::Deadline))
        );
        assert_eq!(fresh.remaining(), before);
    }
    #[test]
    fn budgeted_job_byte_and_prepaid_record_cuts_are_sticky() {
        let source = b"filename*13*";
        for bytes in [true, false] {
            let amount = if bytes {
                source.len() as u64
            } else {
                (2 * source.len() as u64 + 1).div_ceil(16)
            };
            for limit in 0..amount {
                let caps = Charge {
                    io_bytes: if bytes { limit } else { source.len() as u64 },
                    records: if bytes { amount + 1 } else { limit },
                    ..Charge::default()
                };
                let mut admission = Meter::new(Deadline::after(Tick(0), 100).unwrap(), caps);
                let mut budget = HeaderBudget::new();
                let mut cursor = Budgeted::new(source, &mut admission, &mut budget);
                loop {
                    match cursor.poll(Tick(1)) {
                        Ok(Status::Yield) => {}
                        Ok(Status::Complete(_)) => panic!("partial job admitted"),
                        Err(error) => {
                            let expected = if bytes { Stop::IoBytes } else { Stop::Records };
                            assert_eq!(error, Error::Work(expected));
                            let before = (
                                cursor.work.remaining(),
                                cursor.budget.steps_remaining(),
                                cursor.budget.source_bytes_remaining(),
                            );
                            assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                            assert_eq!(cursor.poll(Tick(1)), Err(error));
                            assert_eq!(
                                before,
                                (
                                    cursor.work.remaining(),
                                    cursor.budget.steps_remaining(),
                                    cursor.budget.source_bytes_remaining()
                                )
                            );
                            break;
                        }
                    }
                }
            }
        }
    }
}
