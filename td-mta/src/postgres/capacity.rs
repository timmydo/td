//! Bounded, passive capacity observations. No message or credential contents.
use super::{configuration_snapshot, report_error, with_connection, AttemptError};
use std::{
    ffi::OsString,
    io::{self, Write},
    process::ExitCode,
};
use td_mta::postgres_input::Loaded;

const NAME: &str = "postgres-capacity-inspect";
fn failed() -> AttemptError {
    AttemptError::Failed("capacity", "inspection-unavailable")
}

pub(super) fn run(args: impl Iterator<Item = OsString>) -> io::Result<ExitCode> {
    let Some((connection, epoch)) = configuration_snapshot::arguments(args) else {
        report_error(NAME, "arguments", "invalid-arguments")?;
        return Ok(ExitCode::from(2));
    };
    let loaded = match Loaded::open(&connection, "capacity") {
        Ok(value) => value,
        Err(e) => {
            report_error(NAME, "input", e.code())?;
            return Ok(ExitCode::FAILURE);
        }
    };
    let result = with_connection(&loaded, |request, deadline| {
        td_mta::postgres_capacity::inspect(request, deadline, epoch).map_err(|_| failed())
    });
    match result {
        Ok(value) => {
            writeln!(io::stdout().lock(), "{value}")?;
            Ok(ExitCode::SUCCESS)
        }
        Err((stage, code)) => {
            report_error(NAME, stage, code)?;
            Ok(ExitCode::FAILURE)
        }
    }
}
