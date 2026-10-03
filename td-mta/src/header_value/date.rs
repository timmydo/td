use super::{projection::Projection, Error, Form};
use crate::{
    admission::work::{Charge, Meter},
    header_date::{self, project, Date as ParsedDate},
    json_string::{Frame, Progress, Status},
    nfc::HeaderBudget,
    ports::Tick,
};
pub(super) struct DateMode;
impl<'a, 'w> Projection<'a, 'w> for DateMode {
    type Source = Source<'a, 'w>;
    type Workspace = ();
    const FORM: Form = Form::Date;
    fn validate(
        _: &str,
        _: Tick,
        _: &mut Meter,
        _: &mut HeaderBudget,
        _: &mut (),
    ) -> Result<(), Error> {
        Ok(())
    }
    fn start(
        bytes: &'a [u8],
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
        (): (),
    ) -> Self::Source {
        Source {
            owner: Owner::Parse(header_date::Budgeted::new(bytes, work, budget)),
            phase: Phase::Parse,
            bytes: [0; 27],
            used: 0,
            position: 0,
            unverified: false,
            failure: None,
        }
    }
    fn finish(source: Self::Source) -> Result<(&'w mut Meter, &'w mut HeaderBudget, ()), Error> {
        if let Some(error) = source.failure {
            return Err(error);
        }
        if !matches!(source.phase, Phase::Done) {
            return Err(Error::InvalidState);
        }
        let Owner::Budgets(work, budget) = source.owner else {
            return Err(Error::InvalidState);
        };
        Ok((work, budget, ()))
    }
    fn charge_output(source: &mut Self::Source, now: Tick, bytes: u64) -> Result<(), Error> {
        source.charge_output(now, bytes)
    }
    fn poll(
        source: &mut Self::Source,
        _: &mut Frame,
        now: Tick,
        output: &mut [u8],
    ) -> Result<Progress, Error> {
        source.poll(now, output)
    }
    fn is_encoding_problem(_: &Self::Source) -> bool {
        false
    }
    fn has_unverified_leap(source: &Self::Source) -> bool {
        source.unverified
    }
}
enum Owner<'a, 'w> {
    Parse(header_date::Budgeted<'a, 'w>),
    Budgets(&'w mut Meter, &'w mut HeaderBudget),
    Retired,
}
#[derive(Clone, Copy)]
enum Phase {
    Parse,
    Render(Option<ParsedDate>),
    Drain,
    Done,
}
pub(super) struct Source<'a, 'w> {
    owner: Owner<'a, 'w>,
    phase: Phase,
    bytes: [u8; 27],
    used: usize,
    position: usize,
    unverified: bool,
    failure: Option<Error>,
}
impl Source<'_, '_> {
    fn charge_output(&mut self, now: Tick, output_bytes: u64) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = match &mut self.owner {
            Owner::Parse(source) => source.charge_output(now, output_bytes).map_err(Error::Date),
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
    fn null(&mut self, now: Tick) -> Result<(), Error> {
        self.charge_output(now, 4)?;
        self.bytes
            .get_mut(..4)
            .ok_or(Error::InvalidState)?
            .copy_from_slice(b"null");
        self.used = 4;
        self.phase = Phase::Drain;
        Ok(())
    }
    fn render(&mut self, date: ParsedDate, now: Tick) -> Result<(), Error> {
        let Owner::Budgets(work, budget) = &mut self.owner else {
            return Err(Error::InvalidState);
        };
        let outcome = project::render_with_budget(
            date,
            self.bytes.get_mut(1..26).ok_or(Error::InvalidState)?,
            now,
            work,
            budget,
        )
        .map_err(Error::DateProjection)?;
        let count = match outcome {
            project::Outcome::Date(text) => Some(text.len()),
            project::Outcome::OutOfRange => None,
            project::Outcome::LeapSecondUnverified => {
                self.unverified = true;
                None
            }
        };
        let Some(count) = count else {
            return self.null(now);
        };
        // The formatter emits only fixed-width ASCII date syntax, safe inside JSON quotes.
        self.charge_output(now, 2)?;
        let closing = count.checked_add(1).ok_or(Error::InvalidState)?;
        *self.bytes.first_mut().ok_or(Error::InvalidState)? = b'"';
        *self.bytes.get_mut(closing).ok_or(Error::InvalidState)? = b'"';
        self.used = closing.checked_add(1).ok_or(Error::InvalidState)?;
        self.phase = Phase::Drain;
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
        self.charge_output(now, 0)?;
        if output.is_empty() {
            return Ok(Progress {
                written: 0,
                status: Status::NeedOutput,
            });
        }
        match self.phase {
            Phase::Parse => {
                let Owner::Parse(source) = &mut self.owner else {
                    return Err(Error::InvalidState);
                };
                if let header_date::Status::Complete(date) =
                    source.poll(now).map_err(Error::Date)?
                {
                    let Owner::Parse(source) = std::mem::replace(&mut self.owner, Owner::Retired)
                    else {
                        return Err(Error::InvalidState);
                    };
                    let (work, budget) = source.finish().map_err(Error::Date)?;
                    self.owner = Owner::Budgets(work, budget);
                    self.phase = Phase::Render(date);
                }
            }
            Phase::Render(None) => self.null(now)?,
            Phase::Render(Some(date)) => self.render(date, now)?,
            Phase::Drain => {
                let count = self
                    .used
                    .checked_sub(self.position)
                    .ok_or(Error::InvalidState)?
                    .min(output.len())
                    .min(6);
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
                    self.phase = Phase::Done;
                }
                return Ok(Progress {
                    written: count,
                    status: if end == self.used {
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
    #[test]
    fn handoff_refuses_render_and_staged_drain_owners() {
        for render in [true, false] {
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: 100_000,
                    records: 100_000,
                    output_bytes: 1000,
                    ..Charge::default()
                },
            );
            let mut budget = HeaderBudget::new();
            let mut source = DateMode::start(b"1 Jan 2000 00:00 +0000", &mut work, &mut budget, ());
            let mut frame = Frame::new();
            let mut reached = false;
            for _ in 0..1000 {
                DateMode::poll(&mut source, &mut frame, Tick(1), &mut [0; 1]).unwrap();
                if (render && matches!(source.phase, Phase::Render(_)))
                    || (!render && matches!(source.phase, Phase::Drain))
                {
                    reached = true;
                    break;
                }
            }
            assert!(reached);
            assert!(matches!(source.owner, Owner::Budgets(_, _)));
            assert!(matches!(DateMode::finish(source), Err(Error::InvalidState)));
        }
    }
}
