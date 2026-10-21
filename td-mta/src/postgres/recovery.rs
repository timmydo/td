//! Explicit restored-store release; callers retain both epochs before mutation.
use super::{report_error, with_addresses, AttemptError};
use std::{
    ffi::OsString,
    io::{self, Write},
    process::ExitCode,
    time::{SystemTime, UNIX_EPOCH},
};
use td_mta::{
    ids::StoreEpoch,
    postgres_input::Loaded,
    postgres_recovery::{self, Error, ExternalChecks, Plan},
    postgres_schema,
};
const NAME: &str = "postgres-recovery-release";
struct Options {
    connection: String,
    old: StoreEpoch,
    new: StoreEpoch,
    rows: u32,
}
fn arguments(mut args: impl Iterator<Item = OsString>) -> Option<Options> {
    let (mut connection, mut old, mut new, mut rows) = (None, None, None, None);
    let (mut fenced, mut reviewed) = (false, false);
    while let Some(flag) = args.next() {
        match flag.to_str()? {
            "--connection-file" if connection.is_none() => {
                connection = Some(args.next()?.into_string().ok()?)
            }
            "--epoch" if old.is_none() => {
                old = Some(StoreEpoch::parse(&args.next()?.into_string().ok()?).ok()?)
            }
            "--new-epoch" if new.is_none() => {
                new = Some(StoreEpoch::parse(&args.next()?.into_string().ok()?).ok()?)
            }
            "--max-rows" if rows.is_none() => {
                rows = Some(args.next()?.into_string().ok()?.parse::<u32>().ok()?)
            }
            "--instances-and-smtp-fenced" if !fenced => fenced = true,
            "--configuration-reviewed" if !reviewed => reviewed = true,
            _ => return None,
        }
    }
    let (old, new, rows) = (old?, new?, rows?);
    if !fenced
        || !reviewed
        || old == new
        || new.as_bytes() == &[0; 16]
        || !(1..=1_000_000).contains(&rows)
    {
        return None;
    }
    Some(Options {
        connection: connection?,
        old,
        new,
        rows,
    })
}
pub(super) fn run(args: impl Iterator<Item = OsString>) -> io::Result<ExitCode> {
    let Some(options) = arguments(args) else {
        report_error(NAME, "arguments", "invalid-arguments")?;
        return Ok(ExitCode::from(2));
    };
    let loaded = match Loaded::open(&options.connection, "recovery") {
        Ok(loaded) => loaded,
        Err(error) => {
            report_error(NAME, "bootstrap", &error.to_string())?;
            return Ok(ExitCode::FAILURE);
        }
    };
    let utc_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok());
    let Some(utc_ms) = utc_ms else {
        report_error(NAME, "clock", "unavailable")?;
        return Ok(ExitCode::FAILURE);
    };
    let plan = Plan {
        old_epoch: options.old,
        new_epoch: options.new,
        utc_ms,
        maximum_rows: options.rows,
    };
    let result = with_addresses(&loaded, |request, deadline| {
        postgres_recovery::release(
            request,
            &loaded.trust,
            deadline,
            plan,
            ExternalChecks::InstancesAndSmtpFencedAndConfigurationReviewed,
        )
        .map_err(classify)
    });
    match result {
        Ok(report) => {
            writeln!(io::stdout().lock(),"{{\"schema\":1,\"command\":\"{NAME}\",\"status\":\"ok\",\"store_epoch\":\"{}\",\"quarantined_recipients\":{},\"completed_submissions\":{},\"blocked\":false,\"services_started\":false}}",report.epoch,report.quarantined_recipients,report.completed_submissions)?;
            Ok(ExitCode::SUCCESS)
        }
        Err((stage, code)) => {
            writeln!(io::stdout().lock(),"{{\"schema\":1,\"command\":\"{NAME}\",\"status\":\"error\",\"stage\":\"{stage}\",\"error\":\"{code}\",\"old_epoch\":\"{}\",\"new_epoch\":\"{}\"}}",options.old,options.new)?;
            Ok(ExitCode::FAILURE)
        }
    }
}
fn classify(error: Error) -> AttemptError {
    let code = match error {
        Error::Schema(error) => return AttemptError::from(error),
        Error::Connection(error) => {
            return AttemptError::from(postgres_schema::Error::Connection(error))
        }
        Error::Maintenance(_) => "maintenance-owner-or-integrity",
        Error::Input => "invalid-input",
        Error::Epoch => "epoch-mismatch",
        Error::BarrierRequired => "maintenance-required",
        Error::ActiveCredentials => "revoke-restored-devices-first",
        Error::Capacity => "row-limit",
        Error::Integrity => "integrity",
        Error::Database => "query-failed",
        Error::Indeterminate => "indeterminate-inspect-new-epoch",
    };
    AttemptError::Failed("recovery", code)
}
#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing)]
    use super::*;
    #[test]
    fn requires_distinct_epochs_bounds_and_both_attestations() {
        let base = [
            "--connection-file",
            "connection",
            "--epoch",
            "11111111111111111111111111111111",
            "--new-epoch",
            "22222222222222222222222222222222",
            "--max-rows",
            "100",
            "--instances-and-smtp-fenced",
            "--configuration-reviewed",
        ];
        let parse = |v: &[&str]| arguments(v.iter().map(OsString::from));
        assert!(parse(&base).is_some());
        for omitted in [8, 9] {
            let mut v = base.to_vec();
            v.remove(omitted);
            assert!(parse(&v).is_none());
        }
        for bad in ["0", "1000001", "invalid"] {
            let mut v = base.to_vec();
            v[7] = bad;
            assert!(parse(&v).is_none());
        }
        let mut v = base.to_vec();
        v[5] = v[3];
        assert!(parse(&v).is_none());
        let mut v = base.to_vec();
        v.push("--configuration-reviewed");
        assert!(parse(&v).is_none());
    }
}
