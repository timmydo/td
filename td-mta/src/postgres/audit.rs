//! Explicit historical account inspection with caller-supplied bounds.
use super::{account, report_error, with_addresses, AttemptError};
use std::{
    ffi::OsString,
    io::{self, Write},
    process::ExitCode,
    time::{SystemTime, UNIX_EPOCH},
};
use td_mta::{
    ids::{AccountId, StoreEpoch},
    metadata_sweep, postgres_account_audit as audit,
    postgres_input::Loaded,
    postgres_metadata_audit, postgres_schema, postgres_usage_audit,
};
const COMMAND: &str = "postgres-account-audit";
struct Options {
    account: account::Options,
    limits: audit::Limits,
}
fn unsigned(value: &str) -> Option<u64> {
    if value.is_empty() || value.len() > 20 || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}
fn arguments(mut args: impl Iterator<Item = OsString>) -> Option<Options> {
    let (mut connection, mut epoch, mut account) = (None, None, None);
    let (mut rows, mut parents, mut blobs, mut bytes) = (None, None, None, None);
    while let Some(flag) = args.next() {
        let value = args.next()?.into_string().ok()?;
        match flag.to_str()? {
            "--connection-file" if connection.is_none() => connection = Some(value),
            "--epoch" if epoch.is_none() => epoch = Some(StoreEpoch::parse(&value).ok()?),
            "--account" if account.is_none() => account = Some(AccountId::parse(&value).ok()?),
            "--max-rows" if rows.is_none() => rows = Some(unsigned(&value)?),
            "--max-parent-reads" if parents.is_none() => parents = Some(unsigned(&value)?),
            "--max-blobs" if blobs.is_none() => blobs = Some(unsigned(&value)?),
            "--max-bytes" if bytes.is_none() => bytes = Some(unsigned(&value)?),
            _ => return None,
        }
    }
    Some(Options {
        account: account::Options {
            connection: connection?,
            epoch: epoch?,
            account: account?,
        },
        limits: audit::Limits {
            metadata: metadata_sweep::Limits {
                rows: rows?,
                parent_reads: parents?,
            },
            blobs: blobs?,
            bytes: bytes?,
        },
    })
}
pub(super) fn run(args: impl Iterator<Item = OsString>) -> io::Result<ExitCode> {
    let Some(options) = arguments(args) else {
        report_error(COMMAND, "arguments", "invalid-arguments")?;
        return Ok(ExitCode::from(2));
    };
    let loaded = match Loaded::open(&options.account.connection, "audit") {
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
            let metadata = report.metadata();
            let identity = metadata.references().identity();
            let usage = report.usage();
            writeln!(io::stdout().lock(),"{{\"schema\":1,\"command\":\"{COMMAND}\",\"status\":\"ok\",\"store_epoch\":\"{}\",\"account\":\"{}\",\"sequence\":{},\"history_floor\":{},\"metadata_rows\":{},\"verified_blobs\":{},\"verified_bytes\":{},\"body_bytes\":{},\"blob_count\":{},\"upload_bytes\":{},\"queue_bytes\":{},\"queue_submissions\":{},\"canonical_metadata_verified\":true,\"counters_consistent\":true,\"body_digests_verified\":true,\"semantic_verified\":false}}",identity.epoch,identity.account,identity.committed_sequence.number(),identity.history_floor.number(),metadata.references().rows(),report.blobs(),report.bytes(),usage.body_bytes,usage.blob_count,usage.upload_bytes,usage.queue_bytes,usage.queue_submissions)?;
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
        Error::Metadata(postgres_metadata_audit::Error::Connection(error)) => {
            return AttemptError::from(postgres_schema::Error::Connection(error))
        }
        Error::Metadata(postgres_metadata_audit::Error::Schema(error)) => {
            return AttemptError::from(error)
        }
        Error::Metadata(postgres_metadata_audit::Error::Epoch) => "epoch-mismatch",
        Error::Metadata(postgres_metadata_audit::Error::Missing) => "account-not-found",
        Error::Metadata(postgres_metadata_audit::Error::Database) => "query-failed",
        Error::Metadata(_) => "metadata-check-failed",
        Error::Usage(postgres_usage_audit::Error::Database) => "query-failed",
        Error::Usage(_) => "usage-integrity",
        Error::Body(td_mta::postgres_body::Error::Connection(error)) => {
            return AttemptError::from(postgres_schema::Error::Connection(error))
        }
        Error::Body(td_mta::postgres_body::Error::Read(td_mta::ports::Error::Deadline)) => {
            "deadline"
        }
        Error::Body(_) => "body-check-failed",
        Error::Capacity => "body-limit",
        Error::Integrity => "account-integrity",
        Error::Database => "query-failed",
    };
    AttemptError::Failed("audit", code)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decimal_limits_are_explicit_and_unsigned() {
        assert_eq!(unsigned("0"), Some(0));
        assert_eq!(unsigned("18446744073709551615"), Some(u64::MAX));
        for value in [
            "",
            "+1",
            "-1",
            " 1",
            "1 ",
            "18446744073709551616",
            "000000000000000000000",
            "１",
        ] {
            assert_eq!(unsigned(value), None);
        }
        let base = [
            "--connection-file",
            "/private/db",
            "--epoch",
            "00000000000000000000000000000000",
            "--account",
            "01010101010101010101010101010101",
            "--max-rows",
            "10",
            "--max-parent-reads",
            "10",
            "--max-blobs",
            "0",
            "--max-bytes",
            "0",
        ];
        assert!(arguments(base.iter().map(OsString::from)).is_some());
        for cut in 0..base.len() {
            assert!(arguments(base.iter().take(cut).map(OsString::from)).is_none());
        }
        for extra in [
            "--max-rows",
            "--max-parent-reads",
            "--max-blobs",
            "--max-bytes",
            "--repair",
            "--profile",
            "--all",
        ] {
            assert!(
                arguments(base.iter().copied().chain([extra, "1"]).map(OsString::from)).is_none()
            );
        }
    }
}
