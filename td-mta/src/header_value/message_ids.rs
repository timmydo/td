use super::{projection::Projection, Error, Form};
use crate::{
    admission::work::{Charge, Meter},
    header_message_ids::{
        self as ids,
        project::{Budgeted, Status as Converted},
        Mode,
    },
    json_string::{self, Frame, Progress, ScalarSource, Status},
    nfc::{self, HeaderBudget},
    ports::Tick,
};
pub(super) struct IdsMode;
impl<'a, 'w> Projection<'a, 'w> for IdsMode {
    type Source = Source<'a, 'w>;
    type Workspace = Mode;
    const FORM: Form = Form::MessageIds;
    fn validate(
        name: &str,
        now: Tick,
        work: &mut Meter,
        budget: &mut HeaderBudget,
        mode: &mut Mode,
    ) -> Result<(), Error> {
        let field = match name.len() {
            10 => Some("References"),
            11 => Some("In-Reply-To"),
            _ => None,
        };
        budget.charge(
            work,
            now,
            field.map_or(0, |field| field.len() as u64),
            1,
            &mut 0,
        )?;
        *mode = if field.is_some_and(|field| name.eq_ignore_ascii_case(field)) {
            Mode::ObsoletePhrases
        } else {
            Mode::Strict
        };
        Ok(())
    }
    fn start(
        bytes: &'a [u8],
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
        mode: Mode,
    ) -> Self::Source {
        Source {
            owner: Owner::Convert(Budgeted::new(bytes, mode, work, budget)),
            mode,
            phase: Phase::First,
            next: Phase::First,
            bytes: [0; 4],
            used: 0,
            position: 0,
            failure: None,
        }
    }
    fn finish(source: Self::Source) -> Result<(&'w mut Meter, &'w mut HeaderBudget, Mode), Error> {
        if let Some(error) = source.failure {
            return Err(error);
        }
        if !matches!(source.phase, Phase::Done) {
            return Err(Error::InvalidState);
        }
        let (work, budget) = match source.owner {
            Owner::Convert(cursor) => cursor.finish().map_err(Error::MessageIds)?,
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
        frame: &mut Frame,
        now: Tick,
        output: &mut [u8],
    ) -> Result<Progress, Error> {
        source.poll(frame, now, output)
    }
    fn is_encoding_problem(source: &Self::Source) -> bool {
        match &source.owner {
            Owner::Convert(cursor) => cursor.is_encoding_problem(),
            _ => false,
        }
    }
}
#[allow(clippy::large_enum_variant)] // Inline state uses the fixed parser reservation.
enum Owner<'a, 'w> {
    Convert(Budgeted<'a, 'w>),
    Budgets(&'w mut Meter, &'w mut HeaderBudget),
    Retired,
}
#[derive(Clone, Copy)]
enum Phase {
    First,
    ItemStart,
    Item,
    Between,
    Drain,
    Done,
}
pub(super) struct Source<'a, 'w> {
    owner: Owner<'a, 'w>,
    mode: Mode,
    phase: Phase,
    next: Phase,
    bytes: [u8; 4],
    used: usize,
    position: usize,
    failure: Option<Error>,
}
struct Item<'c, 'a, 'w>(&'c mut Budgeted<'a, 'w>);
impl ScalarSource for Item<'_, '_, '_> {
    fn charge_output(&mut self, now: Tick, bytes: u64) -> Result<(), json_string::Error> {
        self.0
            .charge_output(now, bytes)
            .map_err(json_string::Error::MessageIds)
    }
    fn poll(&mut self, now: Tick) -> Result<nfc::Status, json_string::Error> {
        match self.0.poll(now).map_err(json_string::Error::MessageIds)? {
            Converted::Yield => Ok(nfc::Status::Yield),
            Converted::Scalar(value) => Ok(nfc::Status::Scalar(value)),
            Converted::End => Ok(nfc::Status::Complete),
            Converted::Begin | Converted::Complete => Err(json_string::Error::InvalidState),
        }
    }
}
impl Source<'_, '_> {
    #[cfg(test)]
    pub(super) fn remaining(&self) -> Option<(Charge, u64)> {
        match &self.owner {
            Owner::Convert(cursor) => Some(cursor.remaining()),
            Owner::Budgets(work, budget) => Some((work.remaining(), budget.steps_remaining())),
            Owner::Retired => None,
        }
    }

    fn charge_output(&mut self, now: Tick, output_bytes: u64) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = match &mut self.owner {
            Owner::Convert(cursor) => cursor
                .charge_output(now, output_bytes)
                .map_err(Error::MessageIds),
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
    fn stage(&mut self, now: Tick, bytes: &[u8], next: Phase) -> Result<(), Error> {
        if bytes.is_empty() || bytes.len() > self.bytes.len() {
            return Err(Error::InvalidState);
        }
        self.charge_output(now, bytes.len() as u64)?;
        self.bytes
            .get_mut(..bytes.len())
            .ok_or(Error::InvalidState)?
            .copy_from_slice(bytes);
        self.used = bytes.len();
        self.position = 0;
        self.next = next;
        self.phase = Phase::Drain;
        Ok(())
    }
    fn poll(&mut self, frame: &mut Frame, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.phase, Phase::Done) {
            return Ok(Progress {
                written: 0,
                status: Status::Complete,
            });
        }
        let result = self.step(frame, now, output);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn step(&mut self, frame: &mut Frame, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        self.charge_output(now, 0)?;
        if output.is_empty() {
            return Ok(Progress {
                written: 0,
                status: Status::NeedOutput,
            });
        }
        match self.phase {
            Phase::First | Phase::Between => {
                let Owner::Convert(cursor) = &mut self.owner else {
                    return Err(Error::InvalidState);
                };
                match cursor.poll(now) {
                    Ok(Converted::Yield) => {}
                    Ok(Converted::Begin) => {
                        let bytes = if matches!(self.phase, Phase::First) {
                            b"["
                        } else {
                            b","
                        };
                        self.stage(now, bytes, Phase::ItemStart)?;
                    }
                    Ok(Converted::Complete) => {
                        let bytes: &[u8] = if matches!(self.phase, Phase::First) {
                            b"[]"
                        } else {
                            b"]"
                        };
                        self.stage(now, bytes, Phase::Done)?;
                    }
                    Err(ids::Error::Malformed) if matches!(self.phase, Phase::First) => {
                        let Owner::Convert(cursor) =
                            std::mem::replace(&mut self.owner, Owner::Retired)
                        else {
                            return Err(Error::InvalidState);
                        };
                        let (work, budget) =
                            cursor.finish_malformed().map_err(Error::MessageIds)?;
                        self.owner = Owner::Budgets(work, budget);
                        self.stage(now, b"null", Phase::Done)?;
                    }
                    Err(error) => return Err(Error::MessageIds(error)),
                    Ok(Converted::Scalar(_) | Converted::End) => return Err(Error::InvalidState),
                }
            }
            Phase::ItemStart => {
                *frame = Frame::new();
                self.phase = Phase::Item;
            }
            Phase::Item => {
                let Owner::Convert(cursor) = &mut self.owner else {
                    return Err(Error::InvalidState);
                };
                let progress = frame
                    .poll(&mut Item(cursor), now, output)
                    .map_err(Error::Json)?;
                if progress.status == Status::Complete {
                    self.phase = Phase::Between;
                }
                return Ok(Progress {
                    written: progress.written,
                    status: Status::Yield,
                });
            }
            Phase::Drain => {
                let count = self
                    .used
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
                    .copy_from_slice(
                        self.bytes
                            .get(self.position..end)
                            .ok_or(Error::InvalidState)?,
                    );
                self.position = end;
                if end == self.used {
                    self.phase = self.next;
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
    use crate::ports::Deadline;
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
    fn source_handoff_requires_complete_drained_value() {
        for (bytes, target) in [
            (b"<a@b>".as_slice(), Phase::First),
            (b"<a@b>", Phase::Drain),
            (b"<a@b>", Phase::Item),
            (b"<a@b>", Phase::Between),
            (b"bad", Phase::Drain),
        ] {
            let mut work = work();
            let mut budget = HeaderBudget::new();
            let mut source = IdsMode::start(bytes, &mut work, &mut budget, Mode::Strict);
            let mut frame = Frame::new();
            let mut reached = false;
            for _ in 0..1000 {
                if std::mem::discriminant(&source.phase) == std::mem::discriminant(&target) {
                    reached = true;
                    break;
                }
                IdsMode::poll(&mut source, &mut frame, Tick(1), &mut [0; 1]).unwrap();
            }
            assert!(reached);
            if bytes == b"bad" {
                assert!(matches!(source.owner, Owner::Budgets(_, _)));
                assert_eq!(
                    IdsMode::poll(&mut source, &mut frame, Tick(1), &mut [0; 1])
                        .unwrap()
                        .written,
                    1
                );
            }
            assert!(matches!(IdsMode::finish(source), Err(Error::InvalidState)));
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
        for _ in 0..1000 {
            if cursor.poll(Tick(1)).unwrap() == Converted::Complete {
                break;
            }
        }
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
}
