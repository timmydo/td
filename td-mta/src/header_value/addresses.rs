use super::{projection::Projection, Error, Form};
use crate::{
    admission::work::{Charge, Meter},
    decode_work::Parsing,
    header_address_text as text, header_addresses as parse,
    header_mailbox::Name,
    header_name,
    json_string::{self, Frame, Progress, Status},
    nfc::{HeaderBudget, Scratch},
    ports::Tick,
};
pub(super) struct AddressMode;
impl<'a, 'w> Projection<'a, 'w> for AddressMode {
    type Source = Source<'a, 'w>;
    type Workspace = &'w mut Scratch;
    const FORM: Form = Form::Addresses;
    fn validate(
        _: &str,
        _: Tick,
        _: &mut Meter,
        _: &mut HeaderBudget,
        _: &mut Self::Workspace,
    ) -> Result<(), Error> {
        Ok(())
    }
    fn start(
        input: &'a [u8],
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
        scratch: Self::Workspace,
    ) -> Self::Source {
        Source {
            input,
            parser: parse::Cursor::new(input),
            owner: Owner::Budgets(work, budget, scratch),
            phase: Phase::Open,
            next: Phase::Open,
            mailbox: None,
            seen: false,
            problem: false,
            credit: 0,
            literal: [0; 9],
            used: 0,
            position: 0,
            failure: None,
        }
    }
    fn finish(
        source: Self::Source,
    ) -> Result<(&'w mut Meter, &'w mut HeaderBudget, Self::Workspace), Error> {
        if let Some(error) = source.failure {
            return Err(error);
        }
        if !matches!(source.phase, Phase::Done) {
            return Err(Error::InvalidState);
        }
        let Owner::Budgets(work, budget, scratch) = source.owner else {
            return Err(Error::InvalidState);
        };
        Ok((work, budget, scratch))
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
        source.problem
    }
}
// Keep each active conversion inline beside the suspended list parser.
#[allow(clippy::large_enum_variant)]
enum Owner<'a, 'w> {
    Budgets(&'w mut Meter, &'w mut HeaderBudget, &'w mut Scratch),
    Name(header_name::Cursor<'a, 'w>),
    Address(text::Budgeted<'a, 'w>, &'w mut Scratch),
    Retired,
}
#[derive(Clone, Copy)]
enum Phase {
    Open,
    Parse,
    StartName,
    Name,
    FinishName,
    EmailKey,
    StartAddress,
    Address,
    FinishAddress,
    Drain,
    Done,
}
pub(super) struct Source<'a, 'w> {
    input: &'a [u8],
    parser: parse::Cursor<'a>,
    owner: Owner<'a, 'w>,
    phase: Phase,
    next: Phase,
    mailbox: Option<parse::Address>,
    seen: bool,
    problem: bool,
    credit: u8,
    literal: [u8; 9],
    used: usize,
    position: usize,
    failure: Option<Error>,
}
impl Source<'_, '_> {
    #[cfg(test)]
    pub(super) fn remaining(&self) -> Option<(Charge, u64)> {
        match &self.owner {
            Owner::Budgets(work, budget, _) => Some((work.remaining(), budget.steps_remaining())),
            Owner::Name(cursor) => cursor.remaining(),
            Owner::Address(cursor, _) => Some(cursor.remaining()),
            Owner::Retired => None,
        }
    }

    fn charge_output(&mut self, now: Tick, bytes: u64) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = match &mut self.owner {
            Owner::Budgets(work, budget, _) => budget
                .charge(work, now, 0, 0, &mut self.credit)
                .map_err(Error::from)
                .and_then(|()| {
                    work.charge(
                        now,
                        Charge {
                            output_bytes: bytes,
                            ..Charge::default()
                        },
                    )
                    .map_err(Error::Work)
                }),
            Owner::Name(cursor) => cursor.charge_output(now, bytes).map_err(Error::Name),
            Owner::Address(cursor, _) => {
                cursor.charge_output(now, bytes).map_err(Error::AddressText)
            }
            Owner::Retired => Err(Error::InvalidState),
        };
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn stage(&mut self, now: Tick, bytes: &[u8], next: Phase) -> Result<(), Error> {
        if bytes.is_empty() || bytes.len() > self.literal.len() {
            return Err(Error::InvalidState);
        }
        self.charge_output(now, bytes.len() as u64)?;
        self.literal
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
            Phase::Open => self.stage(now, b"[", Phase::Parse)?,
            Phase::Parse => {
                let Owner::Budgets(work, budget, _) = &mut self.owner else {
                    return Err(Error::InvalidState);
                };
                match self
                    .parser
                    .poll_with_work(now, &mut Parsing::new(work, budget, &mut self.credit))
                    .map_err(Error::Addresses)?
                {
                    parse::Status::Yield
                    | parse::Status::BeginGroup(_)
                    | parse::Status::EndGroup => {}
                    parse::Status::Mailbox(mailbox) => {
                        self.mailbox = Some(mailbox);
                        let bytes: &[u8] = if self.seen {
                            b",{\"name\":"
                        } else {
                            b"{\"name\":"
                        };
                        self.stage(now, bytes, Phase::StartName)?;
                        self.seen = true;
                    }
                    parse::Status::Complete => self.stage(now, b"]", Phase::Done)?,
                }
            }
            Phase::StartName => {
                let name = match self.mailbox.ok_or(Error::InvalidState)? {
                    parse::Address::Parsed(mailbox) => mailbox.name,
                    parse::Address::Raw(_) => None,
                };
                if let Some(name) = name {
                    let (extent, kind) = match name {
                        Name::Phrase(extent) => (extent, header_name::Kind::Phrase),
                        Name::Comment(extent) => (extent, header_name::Kind::Comment),
                    };
                    let Owner::Budgets(work, budget, scratch) =
                        std::mem::replace(&mut self.owner, Owner::Retired)
                    else {
                        return Err(Error::InvalidState);
                    };
                    self.owner = Owner::Name(
                        header_name::Cursor::new(self.input, extent, kind, work, budget, scratch)
                            .map_err(Error::Name)?,
                    );
                    *frame = Frame::new();
                    self.phase = Phase::Name;
                } else {
                    self.stage(now, b"null", Phase::EmailKey)?;
                }
            }
            Phase::Name => {
                let Owner::Name(cursor) = &mut self.owner else {
                    return Err(Error::InvalidState);
                };
                let mut progress = frame
                    .poll(&mut json_string::Source::Name(cursor), now, output)
                    .map_err(Error::Json)?;
                if progress.status == Status::Complete {
                    self.phase = Phase::FinishName;
                    progress.status = Status::Yield;
                }
                return Ok(progress);
            }
            Phase::FinishName => {
                let Owner::Name(cursor) = std::mem::replace(&mut self.owner, Owner::Retired) else {
                    return Err(Error::InvalidState);
                };
                self.problem |= cursor.is_encoding_problem();
                let (work, budget, scratch) = cursor.finish().map_err(Error::Name)?;
                self.owner = Owner::Budgets(work, budget, scratch);
                self.phase = Phase::EmailKey;
            }
            Phase::EmailKey => self.stage(now, b",\"email\":", Phase::StartAddress)?,
            Phase::StartAddress => {
                let (extent, mode) = match self.mailbox.ok_or(Error::InvalidState)? {
                    parse::Address::Parsed(mailbox) => (mailbox.address, text::Mode::Parsed),
                    parse::Address::Raw(extent) => (extent, text::Mode::Fallback),
                };
                let input = self
                    .input
                    .get(extent.start..extent.end)
                    .ok_or(Error::InvalidState)?;
                let Owner::Budgets(work, budget, scratch) =
                    std::mem::replace(&mut self.owner, Owner::Retired)
                else {
                    return Err(Error::InvalidState);
                };
                self.owner =
                    Owner::Address(text::Budgeted::new(input, mode, work, budget), scratch);
                *frame = Frame::new();
                self.phase = Phase::Address;
            }
            Phase::Address => {
                let Owner::Address(cursor, _) = &mut self.owner else {
                    return Err(Error::InvalidState);
                };
                let mut progress = frame
                    .poll(
                        &mut json_string::Source::BudgetedAddress(cursor),
                        now,
                        output,
                    )
                    .map_err(Error::Json)?;
                if progress.status == Status::Complete {
                    self.phase = Phase::FinishAddress;
                    progress.status = Status::Yield;
                }
                return Ok(progress);
            }
            Phase::FinishAddress => {
                let Owner::Address(cursor, scratch) =
                    std::mem::replace(&mut self.owner, Owner::Retired)
                else {
                    return Err(Error::InvalidState);
                };
                self.problem |= cursor.is_encoding_problem();
                let (work, budget) = cursor.finish().map_err(Error::AddressText)?;
                self.owner = Owner::Budgets(work, budget, scratch);
                self.mailbox = None;
                self.stage(now, b"}", Phase::Parse)?;
            }
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
                        self.literal
                            .get(self.position..end)
                            .ok_or(Error::InvalidState)?,
                    );
                self.position = end;
                if self.position == self.used {
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
                output_bytes: 100_000,
                ..Charge::default()
            },
        )
    }
    #[test]
    fn completion_handoff_refuses_active_names_addresses_and_undrained_objects() {
        for target in [
            Phase::Open,
            Phase::Drain,
            Phase::Name,
            Phase::FinishName,
            Phase::Address,
            Phase::FinishAddress,
        ] {
            let mut work = work();
            let mut budget = HeaderBudget::new();
            let mut scratch = Scratch::new();
            let mut source = AddressMode::start(b"Jo <a@b>", &mut work, &mut budget, &mut scratch);
            let mut frame = Frame::new();
            let mut reached = false;
            for _ in 0..1000 {
                if std::mem::discriminant(&source.phase) == std::mem::discriminant(&target) {
                    reached = true;
                    break;
                }
                AddressMode::poll(&mut source, &mut frame, Tick(1), &mut [0; 1]).unwrap();
            }
            assert!(reached);
            assert!(matches!(
                AddressMode::finish(source),
                Err(Error::InvalidState)
            ));
        }
    }
    #[test]
    fn inner_name_and_address_handoffs_require_actual_completion_and_no_failure() {
        let mut work = work();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        assert!(matches!(
            text::Budgeted::new(b"a@b", text::Mode::Parsed, &mut work, &mut budget).finish(),
            Err(text::Error::InvalidState)
        ));
        let mut name = header_name::Cursor::new(
            b"x",
            parse::Extent { start: 0, end: 1 },
            header_name::Kind::Phrase,
            &mut work,
            &mut budget,
            &mut scratch,
        )
        .unwrap();
        let mut scalar = false;
        for _ in 0..1000 {
            if name.poll(Tick(1)).unwrap() == crate::nfc::Status::Scalar('x') {
                scalar = true;
                break;
            }
        }
        assert!(scalar);
        assert!(matches!(
            name.finish(),
            Err(header_name::Error::InvalidState)
        ));
        let mut text = text::Budgeted::new(b"a@b", text::Mode::Parsed, &mut work, &mut budget);
        let mut complete = false;
        for _ in 0..1000 {
            if text.poll(Tick(1)).unwrap() == text::Status::Complete {
                complete = true;
                break;
            }
        }
        assert!(complete);
        assert_eq!(
            text.check_deadline(Tick(100)),
            Err(text::Error::Work(crate::admission::work::Stop::Deadline))
        );
        assert!(matches!(
            text.finish(),
            Err(text::Error::Work(crate::admission::work::Stop::Deadline))
        ));
    }
}
