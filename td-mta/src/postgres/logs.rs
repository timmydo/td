//! Bounded historical diagnostics; never delivery or repair authority.
use super::{report_error, with_addresses, AttemptError};
use std::{
    ffi::OsString,
    io::{self, Write},
    process::ExitCode,
};
use td_mta::{ids::StoreEpoch, postgres_input::Loaded, postgres_logs, postgres_schema};
const COMMAND: &str = "postgres-logs";
struct Options {
    connection: String,
    epoch: StoreEpoch,
    before: Option<i64>,
    limit: usize,
}
fn arguments(mut args: impl Iterator<Item = OsString>) -> Option<Options> {
    let (mut connection, mut epoch, mut before, mut limit) = (None, None, None, None);
    while let Some(flag) = args.next() {
        let value = args.next()?.into_string().ok()?;
        match flag.to_str()? {
            "--connection-file" if connection.is_none() => connection = Some(value),
            "--epoch" if epoch.is_none() => epoch = Some(StoreEpoch::parse(&value).ok()?),
            "--before" if before.is_none() => {
                let n = value.parse::<i64>().ok()?;
                if n <= 0 {
                    return None;
                }
                before = Some(n);
            }
            "--limit" if limit.is_none() => {
                let n = value.parse::<usize>().ok()?;
                if n == 0 || n > postgres_logs::PAGE {
                    return None;
                }
                limit = Some(n);
            }
            _ => return None,
        }
    }
    Some(Options {
        connection: connection?,
        epoch: epoch?,
        before,
        limit: limit.unwrap_or(postgres_logs::PAGE),
    })
}
pub(super) fn run(args: impl Iterator<Item = OsString>) -> io::Result<ExitCode> {
    let Some(options) = arguments(args) else {
        report_error(COMMAND, "arguments", "invalid-arguments")?;
        return Ok(ExitCode::from(2));
    };
    let loaded = match Loaded::open(&options.connection, "logs") {
        Ok(loaded) => loaded,
        Err(error) => {
            report_error(COMMAND, "bootstrap", error.code())?;
            return Ok(ExitCode::FAILURE);
        }
    };
    let result = with_addresses(&loaded, |request, deadline| {
        postgres_logs::inspect(
            request,
            &loaded.trust,
            deadline,
            options.epoch,
            options.before,
            options.limit,
        )
        .map_err(classify)
    });
    match result {
        Ok(page) => {
            let mut records = Vec::with_capacity(page.records.len());
            for row in &page.records {
                let event = match td_json::parse(&row.event) {
                    Ok(event) => event,
                    Err(_) => {
                        report_error(COMMAND, "logs", "invalid-event")?;
                        return Ok(ExitCode::FAILURE);
                    }
                };
                records.push(td_json::json!({"id":row.id,"recorded_ms":row.recorded_ms,"profile":row.profile,"event":event}));
            }
            let next = page.records.last().map(|row| row.id);
            writeln!(
                io::stdout().lock(),
                "{}",
                td_json::json!({"schema":1,"command":COMMAND,"status":"ok","store_epoch":options.epoch.to_string(),"retention_days":30,"max_rows":postgres_logs::MAX_ROWS,"relation_stop_bytes":postgres_logs::MAX_RELATION_BYTES,"relation_bytes":page.relation_bytes,"database_suppressed":page.suppressed,"capacity_evicted":page.evicted,"before":next,"records":records})
            )?;
            Ok(ExitCode::SUCCESS)
        }
        Err((stage, code)) => {
            report_error(COMMAND, stage, code)?;
            Ok(ExitCode::FAILURE)
        }
    }
}
fn classify(error: postgres_logs::Error) -> AttemptError {
    use postgres_logs::Error;
    let code = match error {
        Error::Connect(error) | Error::Connection(error) => {
            return AttemptError::from(postgres_schema::Error::Connection(error))
        }
        Error::Schema(error) => return AttemptError::from(error),
        Error::Resolution => "resolution-failed",
        Error::Epoch => "epoch-mismatch",
        Error::Invalid => "invalid-request",
        Error::Database => "query-failed",
        Error::Integrity => "log-integrity",
    };
    AttemptError::Failed("logs", code)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pages_are_bounded() {
        for value in ["0", "101", "-1", "999999999999999999999"] {
            assert!(arguments(
                [
                    "--connection-file",
                    "x",
                    "--epoch",
                    "00000000000000000000000000000001",
                    "--limit",
                    value
                ]
                .into_iter()
                .map(OsString::from)
            )
            .is_none());
        }
    }
}
