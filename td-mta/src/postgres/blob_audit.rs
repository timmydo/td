//! Historical single-body verification without download authority.
use super::{account, report_error, with_addresses, AttemptError};
use std::{
    ffi::OsString,
    io::{self, Write},
    process::ExitCode,
};
use td_mta::{
    ids::{AccountId, BlobId, StoreEpoch},
    postgres_blob_audit, postgres_body,
    postgres_input::Loaded,
    postgres_schema,
};

const COMMAND: &str = "postgres-blob-audit";

struct Options {
    account: account::Options,
    blob: BlobId,
}
fn arguments(mut args: impl Iterator<Item = OsString>) -> Option<Options> {
    let (mut connection, mut epoch, mut account, mut blob) = (None, None, None, None);
    while let Some(flag) = args.next() {
        let value = args.next()?.into_string().ok()?;
        match flag.to_str()? {
            "--connection-file" if connection.is_none() => connection = Some(value),
            "--epoch" if epoch.is_none() => epoch = Some(StoreEpoch::parse(&value).ok()?),
            "--account" if account.is_none() => account = Some(AccountId::parse(&value).ok()?),
            "--blob" if blob.is_none() => blob = Some(BlobId::parse(&value).ok()?),
            _ => return None,
        }
    }
    Some(Options {
        account: account::Options {
            connection: connection?,
            epoch: epoch?,
            account: account?,
        },
        blob: blob?,
    })
}
pub(super) fn run(args: impl Iterator<Item = OsString>) -> io::Result<ExitCode> {
    let Some(options) = arguments(args) else {
        report_error(COMMAND, "arguments", "invalid-arguments")?;
        return Ok(ExitCode::from(2));
    };
    let loaded = match Loaded::open(&options.account.connection, "blob-audit") {
        Ok(loaded) => loaded,
        Err(error) => {
            report_error(COMMAND, "bootstrap", &error.to_string())?;
            return Ok(ExitCode::FAILURE);
        }
    };
    let result = with_addresses(&loaded, |request, deadline| {
        postgres_blob_audit::inspect(
            request,
            &loaded.trust,
            deadline,
            options.account.epoch,
            options.account.account,
            options.blob,
        )
        .map_err(classify)
    });
    match result {
        Ok(report) => {
            let identity = report.identity;
            writeln!(io::stdout().lock(), "{{\"schema\":1,\"command\":\"{COMMAND}\",\"status\":\"ok\",\"store_epoch\":\"{}\",\"account\":\"{}\",\"blob\":\"{}\",\"sequence\":{},\"history_floor\":{},\"verified_bytes\":{},\"created_at\":{},\"body_digest_verified\":true,\"semantic_verified\":false}}", identity.epoch, identity.account, report.blob, identity.committed_sequence.number(), identity.history_floor.number(), report.body.length, report.body.created_at)?;
            Ok(ExitCode::SUCCESS)
        }
        Err((stage, code)) => {
            report_error(COMMAND, stage, code)?;
            Ok(ExitCode::FAILURE)
        }
    }
}
fn classify(error: postgres_blob_audit::Error) -> AttemptError {
    use postgres_blob_audit::Error;
    let code = match error {
        Error::Connection(error) => {
            return AttemptError::from(postgres_schema::Error::Connection(error));
        }
        Error::Schema(error) => return AttemptError::from(error),
        Error::Epoch => "epoch-mismatch",
        Error::Missing => "blob-not-found",
        Error::Body(postgres_body::Error::Connection(error)) => {
            return AttemptError::from(postgres_schema::Error::Connection(error));
        }
        Error::Body(postgres_body::Error::Read(td_mta::ports::Error::Deadline)) => "deadline",
        Error::Body(_) => "body-check-failed",
        Error::Database => "query-failed",
    };
    AttemptError::Failed("blob-audit", code)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_blob_identity_is_required_before_file_access() {
        let base = [
            "--connection-file",
            "/private/db",
            "--epoch",
            "00000000000000000000000000000000",
            "--account",
            "01010101010101010101010101010101",
            "--blob",
            "03030303030303030303030303030303",
        ];
        assert!(arguments(base.into_iter().map(OsString::from)).is_some());
        for omitted in 0..4 {
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
            "--blob",
            "--repair",
            "--profile",
        ] {
            assert!(arguments(
                base.into_iter()
                    .chain([extra, "03030303030303030303030303030303"])
                    .map(OsString::from)
            )
            .is_none());
        }
        for invalid in ["", "not-an-id", "0303030303030303030303030303030G"] {
            let args = base
                .into_iter()
                .enumerate()
                .map(|(index, value)| OsString::from(if index == 7 { invalid } else { value }));
            assert!(arguments(args).is_none());
        }
    }
}
