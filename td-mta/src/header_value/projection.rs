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
    type Source = (nfc::Cursor<'a, 'w>, crate::header_text::Grammar);
    type Workspace = (&'w mut nfc::Scratch, crate::header_text::Grammar);
    const FORM: Form = Form::Text;
    fn validate(
        name: &str,
        now: Tick,
        work: &mut Meter,
        budget: &mut HeaderBudget,
        workspace: &mut Self::Workspace,
    ) -> Result<(), Error> {
        let fields: &[(&str, crate::header_text::Grammar)] = match name.len() {
            7 => &[
                ("Subject", crate::header_text::Grammar::Text),
                ("List-Id", crate::header_text::Grammar::ListId),
            ],
            8 => &[
                ("Comments", crate::header_text::Grammar::Text),
                ("Keywords", crate::header_text::Grammar::Keywords),
            ],
            12 => &[("Content-Type", crate::header_text::Grammar::MimeParameters)],
            19 => &[
                ("Content-Description", crate::header_text::Grammar::Text),
                (
                    "Content-Disposition",
                    crate::header_text::Grammar::MimeParameters,
                ),
            ],
            _ => &[],
        };
        for &(field, grammar) in fields {
            budget.charge(work, now, field.len() as u64, 1, &mut 0)?;
            if name.eq_ignore_ascii_case(field) {
                workspace.1 = grammar;
                return Ok(());
            }
        }
        let prefix = name.as_bytes().get(..2);
        budget.charge(work, now, prefix.map_or(0, |_| 2), 1, &mut 0)?;
        if prefix.is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"X-")) {
            Ok(())
        } else {
            Err(Error::UnsupportedGrammar)
        }
    }

    fn start(
        bytes: &'a [u8],
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
        (scratch, grammar): Self::Workspace,
    ) -> Self::Source {
        (
            nfc::Cursor::from_header(bytes, grammar, scratch, work, budget),
            grammar,
        )
    }
    fn finish(
        source: Self::Source,
    ) -> Result<(&'w mut Meter, &'w mut HeaderBudget, Self::Workspace), Error> {
        let (source, grammar) = source;
        source
            .finish()
            .map(|(work, budget, scratch)| (work, budget, (scratch, grammar)))
            .map_err(Error::Text)
    }
    fn charge_output(source: &mut Self::Source, now: Tick, bytes: u64) -> Result<(), Error> {
        source.0.charge_output(now, bytes).map_err(Error::Text)
    }
    fn poll(
        source: &mut Self::Source,
        frame: &mut Frame,
        now: Tick,
        output: &mut [u8],
    ) -> Result<Progress, Error> {
        frame
            .poll(&mut Source::Normalized(&mut source.0), now, output)
            .map_err(Error::Json)
    }
    fn is_encoding_problem(source: &Self::Source) -> bool {
        source.0.is_encoding_problem()
    }
}
