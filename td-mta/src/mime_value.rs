//! Provisional MIME parameter octets with original mail admission.
use crate::{
    admission::work::{Meter, Stop},
    decode_work::{self, Lexical, Work},
    nfc::HeaderBudget,
    ports::Tick,
};
pub use td_header::mime_value::{Mode, Role, Status};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Malformed,
    Work(Stop),
    InterpretationLimit,
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => f.write_str("malformed MIME parameter value"),
            Self::Work(error) => write!(f, "MIME value work: {error}"),
            Self::InterpretationLimit => f.write_str("MIME value interpretation limit"),
            Self::InvalidState => f.write_str("invalid MIME value state"),
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
impl From<td_header::mime_value::Error<decode_work::Error>> for Error {
    fn from(error: td_header::mime_value::Error<decode_work::Error>) -> Self {
        match error {
            td_header::mime_value::Error::Malformed => Self::Malformed,
            td_header::mime_value::Error::Work(error) => Self::from(error),
            td_header::mime_value::Error::InvalidState => Self::InvalidState,
        }
    }
}
/// The caller authorizes one complete raw MIME parameter value.
/// Octet events grant no candidate or publication authority.
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {}
/// cloned::<td_mta::mime_value::Cursor<'_>>();
/// ```
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {}
/// copied::<td_mta::mime_value::Cursor<'_>>();
/// ```
pub struct Cursor<'a> {
    inner: td_header::mime_value::Cursor<'a, decode_work::Error>,
}
impl<'a> Cursor<'a> {
    #[must_use]
    pub const fn new(source: &'a [u8], quoted: bool, mode: Mode) -> Self {
        Self {
            inner: td_header::mime_value::Cursor::new(source, quoted, mode),
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
/// Same original job and aggregate work through value completion.
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {}
/// cloned::<td_mta::mime_value::Budgeted<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {}
/// copied::<td_mta::mime_value::Budgeted<'_, '_>>();
/// ```
pub struct Budgeted<'a, 'w> {
    cursor: Cursor<'a>,
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
    credit: u8,
}
impl<'a, 'w> Budgeted<'a, 'w> {
    #[must_use]
    pub fn new(
        source: &'a [u8],
        quoted: bool,
        mode: Mode,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Self {
        Self {
            cursor: Cursor::new(source, quoted, mode),
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
    use crate::{admission::work::Charge, ports::Deadline};
    fn work() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000_000,
                records: 10_000_000,
                output_bytes: 0,
                ..Charge::default()
            },
        )
    }
    #[test]
    fn plain_octets_are_provisional_evidence_and_preserve_extended_bytes() {
        let source = b"\"UTF-8'en'%E2%82%AC%00\"";
        let mut cursor = Cursor::new(source, true, Mode::ExtendedInitial);
        assert!(std::mem::size_of_val(&cursor) <= 160);
        let mut work = work();
        let mut out = [Vec::new(), Vec::new(), Vec::new()];
        loop {
            let before = work.remaining();
            let status = cursor.poll(Tick(1), &mut work).unwrap();
            assert!(before.io_bytes - work.remaining().io_bytes <= 160);
            assert!(before.records - work.remaining().records <= 32);
            assert_eq!(work.remaining().output_bytes, 0);
            match status {
                Status::Yield => {}
                Status::Octet { role, value } => out[match role {
                    Role::Charset => 0,
                    Role::Language => 1,
                    Role::Data => 2,
                }]
                .push(value),
                Status::Complete => break,
            }
        }
        assert_eq!(
            out,
            [
                b"UTF-8".to_vec(),
                b"en".to_vec(),
                b"\xe2\x82\xac\0".to_vec()
            ]
        );
        let before = work.remaining();
        assert_eq!(cursor.poll(Tick(100), &mut work), Ok(Status::Complete));
        assert_eq!(work.remaining(), before);
        assert_eq!(
            cursor.check_deadline(Tick(100), &mut work),
            Err(Error::Work(Stop::Deadline))
        );
        let mut fresh = self::work();
        assert_eq!(
            cursor.poll(Tick(1), &mut fresh),
            Err(Error::Work(Stop::Deadline))
        );
    }
    #[test]
    fn long_budgeted_octet_turns_keep_original_allowances_and_no_output_charge() {
        for pattern in [b"a".as_slice(), "🐈".as_bytes()] {
            let text = if pattern.len() == 1 { "a" } else { "🐈" };
            let source = format!("\"{}\"", text.repeat(4096 / pattern.len()));
            let mut work = work();
            let mut budget = HeaderBudget::new();
            let mut cursor = Budgeted::new(
                source.as_bytes(),
                true,
                Mode::Ordinary,
                &mut work,
                &mut budget,
            );
            assert!(std::mem::size_of_val(&cursor) <= 192);
            let mut count = 0;
            let mut peak = (0, 0, 0);
            loop {
                let before = (
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
                    before
                );
                let status = cursor.poll(Tick(1)).unwrap();
                assert!(before.0.io_bytes - cursor.work.remaining().io_bytes <= 160);
                assert!(before.0.records - cursor.work.remaining().records <= 12);
                assert!(before.1 - cursor.budget.source_bytes_remaining() <= 160);
                assert!(before.2 - cursor.budget.steps_remaining() <= 192);
                peak.0 = peak
                    .0
                    .max(before.1 - cursor.budget.source_bytes_remaining());
                peak.1 = peak.1.max(before.2 - cursor.budget.steps_remaining());
                peak.2 = peak
                    .2
                    .max(before.0.records - cursor.work.remaining().records);
                assert_eq!(cursor.work.remaining().output_bytes, 0);
                match status {
                    Status::Yield => {}
                    Status::Octet {
                        role: Role::Data,
                        value,
                    } => {
                        assert_eq!(value, pattern[count % pattern.len()]);
                        count += 1;
                    }
                    Status::Complete => break,
                    _ => panic!("unexpected event"),
                }
            }
            assert_eq!(count, 4096);
            if pattern.len() == 4 {
                assert_eq!(peak, (160, 192, 12));
            }
            assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
            assert_eq!(
                cursor.check_deadline(Tick(100)),
                Err(Error::Work(Stop::Deadline))
            );
            assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
        }
    }
    #[test]
    fn every_aggregate_and_job_cut_retires_the_complete_derivation() {
        let source = b"plain";
        let visits = 2 * source.len() as u64;
        let steps = 4 * source.len() as u64 + 4;
        for flavor in 0..4 {
            let amount = match flavor {
                0 | 2 => visits,
                1 => steps,
                _ => steps.div_ceil(16),
            };
            for limit in 0..amount {
                let mut work = if flavor >= 2 {
                    Meter::new(
                        Deadline::after(Tick(0), 100).unwrap(),
                        Charge {
                            io_bytes: if flavor == 2 { limit } else { visits },
                            records: if flavor == 3 { limit } else { 100 },
                            ..Charge::default()
                        },
                    )
                } else {
                    self::work()
                };
                let mut budget = HeaderBudget::new();
                let mut credit = 0;
                if flavor < 2 {
                    let (bytes, steps) = if flavor == 0 {
                        (budget.source_bytes_remaining() - limit, 0)
                    } else {
                        (0, budget.steps_remaining() - limit)
                    };
                    budget
                        .charge(&mut work, Tick(1), bytes, steps, &mut credit)
                        .unwrap()
                }
                let mut cursor =
                    Budgeted::new(source, false, Mode::Ordinary, &mut work, &mut budget);
                loop {
                    match cursor.poll(Tick(1)) {
                        Ok(Status::Complete) => panic!("cut admitted completion"),
                        Ok(_) => {}
                        Err(error) => {
                            let expected = match flavor {
                                0 | 1 => Error::InterpretationLimit,
                                2 => Error::Work(Stop::IoBytes),
                                _ => Error::Work(Stop::Records),
                            };
                            assert_eq!(error, expected);
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
    fn malformed_and_final_eof_refusal_cannot_be_retried() {
        let mut work = work();
        let mut cursor = Cursor::new(b"abc%", false, Mode::ExtendedContinuation);
        loop {
            match cursor.poll(Tick(1), &mut work) {
                Ok(Status::Complete) => panic!("malformed completed"),
                Ok(_) => {}
                Err(e) => {
                    assert_eq!(e, Error::Malformed);
                    break;
                }
            }
        }
        assert_eq!(cursor.poll(Tick(1), &mut work), Err(Error::Malformed));
        let mut cursor = Cursor::new(b"a", false, Mode::Ordinary);
        assert_eq!(cursor.poll(Tick(1), &mut work), Ok(Status::Yield));
        assert_eq!(
            cursor.poll(Tick(1), &mut work),
            Ok(Status::Octet {
                role: Role::Data,
                value: b'a'
            })
        );
        assert_eq!(
            cursor.poll(Tick(100), &mut work),
            Err(Error::Work(Stop::Deadline))
        );
        let mut fresh = self::work();
        let before = fresh.remaining();
        assert_eq!(
            cursor.poll(Tick(1), &mut fresh),
            Err(Error::Work(Stop::Deadline))
        );
        assert_eq!(fresh.remaining(), before);
    }
}
