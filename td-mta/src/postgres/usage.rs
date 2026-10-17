//! Historical counter inspection, without repair or serving authority.
use super::{account, report_error, with_addresses, AttemptError};
use std::{
    ffi::OsString,
    io::{self, Write},
    process::ExitCode,
};
use td_mta::{postgres_input::Loaded, postgres_schema, postgres_usage_audit};

const COMMAND: &str = "postgres-usage-inspect";

pub(super) fn run(args: impl Iterator<Item = OsString>) -> io::Result<ExitCode> {
    let Some(options) = account::arguments(args) else {
        report_error(COMMAND, "arguments", "invalid-arguments")?;
        return Ok(ExitCode::from(2));
    };
    let loaded = match Loaded::open(&options.connection, "usage") {
        Ok(loaded) => loaded,
        Err(error) => {
            report_error(COMMAND, "bootstrap", &error.to_string())?;
            return Ok(ExitCode::FAILURE);
        }
    };
    let result = with_addresses(&loaded, |request, deadline| {
        postgres_usage_audit::inspect(
            request,
            &loaded.trust,
            deadline,
            options.epoch,
            options.account,
        )
        .map_err(classify)
    });
    match result {
        Ok(report) => {
            let identity = report.identity;
            let usage = report.usage;
            writeln!(io::stdout().lock(), "{{\"schema\":1,\"command\":\"{COMMAND}\",\"status\":\"ok\",\"store_epoch\":\"{}\",\"account\":\"{}\",\"sequence\":{},\"history_floor\":{},\"body_bytes\":{},\"blob_count\":{},\"upload_bytes\":{},\"queue_bytes\":{},\"queue_submissions\":{},\"counters_consistent\":true,\"semantic_verified\":false}}", identity.epoch, identity.account, identity.committed_sequence.number(), identity.history_floor.number(), usage.body_bytes, usage.blob_count, usage.upload_bytes, usage.queue_bytes, usage.queue_submissions)?;
            Ok(ExitCode::SUCCESS)
        }
        Err((stage, code)) => {
            report_error(COMMAND, stage, code)?;
            Ok(ExitCode::FAILURE)
        }
    }
}
fn classify(error: postgres_usage_audit::Error) -> AttemptError {
    use postgres_usage_audit::Error;
    let code = match error {
        Error::Connection(error) => {
            return AttemptError::from(postgres_schema::Error::Connection(error));
        }
        Error::Schema(error) => return AttemptError::from(error),
        Error::Epoch => "epoch-mismatch",
        Error::Missing => "account-not-found",
        Error::Integrity => "usage-integrity",
        Error::Database => "query-failed",
    };
    AttemptError::Failed("usage", code)
}
