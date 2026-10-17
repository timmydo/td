//! Explicit historical canonical metadata inspection with caller bounds.
use super::{account, audit::unsigned, report_error, with_addresses, AttemptError};
use std::{
    ffi::OsString,
    io::{self, Write},
    process::ExitCode,
    time::{SystemTime, UNIX_EPOCH},
};
use td_mta::{
    ids::{AccountId, StoreEpoch},
    metadata_sweep,
    postgres_input::Loaded,
    postgres_metadata_audit as audit, postgres_schema,
};
const COMMAND: &str = "postgres-metadata-audit";
struct Options {
    account: account::Options,
    limits: metadata_sweep::Limits,
}
fn arguments(mut args: impl Iterator<Item = OsString>) -> Option<Options> {
    let (mut connection, mut epoch, mut account) = (None, None, None);
    let (mut rows, mut parents) = (None, None);
    while let Some(flag) = args.next() {
        let value = args.next()?.into_string().ok()?;
        match flag.to_str()? {
            "--connection-file" if connection.is_none() => connection = Some(value),
            "--epoch" if epoch.is_none() => epoch = Some(StoreEpoch::parse(&value).ok()?),
            "--account" if account.is_none() => account = Some(AccountId::parse(&value).ok()?),
            "--max-rows" if rows.is_none() => rows = Some(unsigned(&value)?),
            "--max-parent-reads" if parents.is_none() => parents = Some(unsigned(&value)?),
            _ => return None,
        }
    }
    Some(Options {
        account: account::Options {
            connection: connection?,
            epoch: epoch?,
            account: account?,
        },
        limits: metadata_sweep::Limits {
            rows: rows?,
            parent_reads: parents?,
        },
    })
}
pub(super) fn run(args: impl Iterator<Item = OsString>) -> io::Result<ExitCode> {
    let Some(options) = arguments(args) else {
        report_error(COMMAND, "arguments", "invalid-arguments")?;
        return Ok(ExitCode::from(2));
    };
    let loaded = match Loaded::open(&options.account.connection, "metadata-audit") {
        Ok(loaded) => loaded,
        Err(error) => {
            report_error(COMMAND, "bootstrap", &error.to_string())?;
            return Ok(ExitCode::FAILURE);
        }
    };
    let Some(utc_ms) = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
    else {
        report_error(COMMAND, "clock", "invalid-time")?;
        return Ok(ExitCode::FAILURE);
    };
    let result = with_addresses(&loaded, |request, deadline| {
        audit::inspect(
            request,
            &loaded.trust,
            deadline,
            options.account.epoch,
            options.account.account,
            utc_ms,
            options.limits,
        )
        .map_err(classify)
    });
    match result {
        Ok(report) => {
            let identity = report.references().identity();
            writeln!(io::stdout().lock(),"{{\"schema\":1,\"command\":\"{COMMAND}\",\"status\":\"ok\",\"store_epoch\":\"{}\",\"account\":\"{}\",\"sequence\":{},\"history_floor\":{},\"metadata_rows\":{},\"mailboxes\":{},\"canonical_metadata_verified\":true,\"body_digests_verified\":false,\"semantic_verified\":false}}",identity.epoch,identity.account,identity.committed_sequence.number(),identity.history_floor.number(),report.references().rows(),report.mailboxes().mailboxes())?;
            Ok(ExitCode::SUCCESS)
        }
        Err((stage, code)) => {
            report_error(COMMAND, stage, code)?;
            Ok(ExitCode::FAILURE)
        }
    }
}
fn classify(error: audit::Error) -> AttemptError {
    use audit::Error;
    let code = match error {
        Error::Connection(error) => {
            return AttemptError::from(postgres_schema::Error::Connection(error))
        }
        Error::Schema(error) => return AttemptError::from(error),
        Error::Epoch => "epoch-mismatch",
        Error::Missing => "account-not-found",
        Error::Integrity => "metadata-integrity",
        Error::Metadata(_) => "metadata-check-failed",
        Error::Database => "query-failed",
    };
    AttemptError::Failed("metadata-audit", code)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_bounds_are_required_and_body_flags_refused() {
        let base = [
            "--connection-file",
            "/private/db",
            "--epoch",
            "00000000000000000000000000000000",
            "--account",
            "01010101010101010101010101010101",
            "--max-rows",
            "100",
            "--max-parent-reads",
            "100",
        ];
        assert!(arguments(base.into_iter().map(OsString::from)).is_some());
        for omitted in 0..5 {
            let args = base
                .as_chunks::<2>()
                .0
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != omitted)
                .flat_map(|(_, pair)| pair.iter().copied())
                .map(OsString::from);
            assert!(arguments(args).is_none());
        }
        for extra in [
            "--connection-file",
            "--epoch",
            "--account",
            "--max-rows",
            "--max-parent-reads",
            "--repair",
            "--max-blobs",
            "--max-bytes",
        ] {
            assert!(arguments(base.into_iter().chain([extra, "1"]).map(OsString::from)).is_none());
        }
        for invalid in [
            "",
            "+1",
            "-1",
            "18446744073709551616",
            "000000000000000000000",
            "１",
        ] {
            for position in [7, 9] {
                let args = base
                    .into_iter()
                    .enumerate()
                    .map(|(i, value)| OsString::from(if i == position { invalid } else { value }));
                assert!(arguments(args).is_none());
            }
        }
        let args = base
            .into_iter()
            .enumerate()
            .map(|(i, value)| OsString::from(if i == 7 || i == 9 { "0" } else { value }));
        assert!(arguments(args).is_some());
    }
}
