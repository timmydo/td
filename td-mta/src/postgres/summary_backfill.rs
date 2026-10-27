//! One finite owner-side summary rebuild page, with an explicit resume cursor.
use super::{report_error, with_addresses, AttemptError};
use std::{
    ffi::OsString,
    io::{self, Write},
    process::ExitCode,
};
use td_mta::{
    ids::{AccountId, BlobId, StoreEpoch},
    postgres_input::Loaded,
    postgres_schema, postgres_summary_backfill,
};

const COMMAND: &str = "postgres-summary-backfill";
struct Options {
    connection: String,
    epoch: StoreEpoch,
    account: AccountId,
    after: Option<BlobId>,
    maximum: usize,
}
fn arguments(mut args: impl Iterator<Item = OsString>) -> Option<Options> {
    let (mut connection, mut epoch, mut account, mut after, mut maximum) =
        (None, None, None, None, None);
    while let Some(flag) = args.next() {
        let value = args.next()?.into_string().ok()?;
        match flag.to_str()? {
            "--connection-file" if connection.is_none() => connection = Some(value),
            "--epoch" if epoch.is_none() => epoch = Some(StoreEpoch::parse(&value).ok()?),
            "--account" if account.is_none() => account = Some(AccountId::parse(&value).ok()?),
            "--after" if after.is_none() => after = Some(BlobId::parse(&value).ok()?),
            "--limit" if maximum.is_none() => {
                maximum = Some(
                    value
                        .parse::<usize>()
                        .ok()
                        .filter(|n| (1..=16).contains(n))?,
                )
            }
            _ => return None,
        }
    }
    Some(Options {
        connection: connection?,
        epoch: epoch?,
        account: account?,
        after,
        maximum: maximum.unwrap_or(16),
    })
}
pub(super) fn run(args: impl Iterator<Item = OsString>) -> io::Result<ExitCode> {
    let Some(options) = arguments(args) else {
        report_error(COMMAND, "arguments", "invalid-arguments")?;
        return Ok(ExitCode::from(2));
    };
    let loaded = match Loaded::open(&options.connection, "summary-backfill") {
        Ok(loaded) => loaded,
        Err(error) => {
            report_error(COMMAND, "bootstrap", &error.to_string())?;
            return Ok(ExitCode::FAILURE);
        }
    };
    let result = with_addresses(&loaded, |request, deadline| {
        postgres_summary_backfill::run(
            request,
            &loaded.trust,
            deadline,
            options.epoch,
            options.account,
            options.after,
            options.maximum,
        )
        .map_err(|error| {
            use postgres_summary_backfill::Error;
            match error {
                Error::Connection(error) => {
                    AttemptError::from(postgres_schema::Error::Connection(error))
                }
                Error::Schema(error) => AttemptError::from(error),
                Error::Body(_) => AttemptError::Failed("summary-backfill", "body-check-failed"),
                Error::Owner => AttemptError::Failed("summary-backfill", "owner-required"),
                Error::Invalid => AttemptError::Failed("summary-backfill", "invalid-state"),
                Error::Database => AttemptError::Failed("summary-backfill", "query-failed"),
                Error::Indeterminate => AttemptError::Failed("summary-backfill", "commit-unknown"),
            }
        })
    });
    match result {
        Ok(report) => {
            let after = report
                .after
                .map(|id| format!("\"{id}\""))
                .unwrap_or_else(|| "null".to_owned());
            writeln!(io::stdout().lock(), "{{\"schema\":1,\"command\":\"{COMMAND}\",\"status\":\"ok\",\"scanned\":{},\"written\":{},\"skipped\":{},\"verified_bytes\":{},\"after\":{after},\"done\":{}}}", report.scanned, report.written, report.skipped, report.verified_bytes, report.done)?;
            Ok(ExitCode::SUCCESS)
        }
        Err((stage, code)) => {
            report_error(COMMAND, stage, code)?;
            Ok(ExitCode::FAILURE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_bounds_are_rejected_before_connection_file_access() {
        for limit in ["0", "17", "-1", "x"] {
            assert!(arguments(
                [
                    "--connection-file",
                    "/private/db",
                    "--epoch",
                    "00000000000000000000000000000000",
                    "--account",
                    "01010101010101010101010101010101",
                    "--limit",
                    limit
                ]
                .into_iter()
                .map(Into::into)
            )
            .is_none());
        }
    }
}
