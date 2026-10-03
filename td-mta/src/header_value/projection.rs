use super::{Error, Form};
use crate::{
    admission::work::Meter,
    header_raw,
    json_string::{Frame, Progress, Source},
    nfc::{self, HeaderBudget},
    ports::Tick,
};
/// Each source retains the original budgets/workspace and hands them back only
/// after successful completion; zero-byte charges remain live deadline checks.
pub(super) trait Projection<'a, 'w> {
    type Source;
    type Workspace;
    const FORM: Form;
    fn validate(
        name: &str,
        now: Tick,
        work: &mut Meter,
        budget: &mut HeaderBudget,
        workspace: &mut Self::Workspace,
    ) -> Result<(), Error>;
    fn start(
        bytes: &'a [u8],
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
        workspace: Self::Workspace,
    ) -> Self::Source;
    fn finish(
        source: Self::Source,
    ) -> Result<(&'w mut Meter, &'w mut HeaderBudget, Self::Workspace), Error>;
    fn charge_output(source: &mut Self::Source, now: Tick, bytes: u64) -> Result<(), Error>;
    fn poll(
        source: &mut Self::Source,
        frame: &mut Frame,
        now: Tick,
        output: &mut [u8],
    ) -> Result<Progress, Error>;
    fn is_encoding_problem(source: &Self::Source) -> bool;
    fn has_unverified_leap(_: &Self::Source) -> bool {
        false
    }
}
pub(super) struct RawMode;
impl<'a, 'w> Projection<'a, 'w> for RawMode {
    type Source = header_raw::Budgeted<'a, 'w>;
    type Workspace = ();
    const FORM: Form = Form::Raw;
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
        header_raw::Budgeted::new(bytes, work, budget)
    }
    fn finish(source: Self::Source) -> Result<(&'w mut Meter, &'w mut HeaderBudget, ()), Error> {
        source
            .finish()
            .map(|(work, budget)| (work, budget, ()))
            .map_err(Error::Raw)
    }
    fn charge_output(source: &mut Self::Source, now: Tick, bytes: u64) -> Result<(), Error> {
        source.charge_output(now, bytes).map_err(Error::Raw)
    }
    fn poll(
        source: &mut Self::Source,
        frame: &mut Frame,
        now: Tick,
        output: &mut [u8],
    ) -> Result<Progress, Error> {
        frame
            .poll(&mut Source::BudgetedRaw(source), now, output)
            .map_err(Error::Json)
    }
    fn is_encoding_problem(source: &Self::Source) -> bool {
        source.is_encoding_problem()
    }
}
pub(super) struct TextMode;
impl<'a, 'w> Projection<'a, 'w> for TextMode {
    type Source = nfc::Cursor<'a, 'w>;
    type Workspace = &'w mut nfc::Scratch;
    const FORM: Form = Form::Text;
    fn validate(
        name: &str,
        now: Tick,
        work: &mut Meter,
        budget: &mut HeaderBudget,
        _: &mut Self::Workspace,
    ) -> Result<(), Error> {
        let field = match name.len() {
            7 => Some("Subject"),
            8 => Some("Comments"),
            _ => None,
        };
        budget.charge(
            work,
            now,
            field.map_or(0, |field| field.len() as u64),
            1,
            &mut 0,
        )?;
        if field.is_some_and(|field| name.eq_ignore_ascii_case(field)) {
            Ok(())
        } else {
            Err(Error::UnsupportedGrammar)
        }
    }
    fn start(
        bytes: &'a [u8],
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
        scratch: Self::Workspace,
    ) -> Self::Source {
        nfc::Cursor::from_unstructured_header(bytes, scratch, work, budget)
    }
    fn finish(
        source: Self::Source,
    ) -> Result<(&'w mut Meter, &'w mut HeaderBudget, Self::Workspace), Error> {
        source.finish().map_err(Error::Text)
    }
    fn charge_output(source: &mut Self::Source, now: Tick, bytes: u64) -> Result<(), Error> {
        source.charge_output(now, bytes).map_err(Error::Text)
    }
    fn poll(
        source: &mut Self::Source,
        frame: &mut Frame,
        now: Tick,
        output: &mut [u8],
    ) -> Result<Progress, Error> {
        frame
            .poll(&mut Source::Normalized(source), now, output)
            .map_err(Error::Json)
    }
    fn is_encoding_problem(source: &Self::Source) -> bool {
        source.is_encoding_problem()
    }
}
