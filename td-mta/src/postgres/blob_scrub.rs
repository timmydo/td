//! Explicit finite scrub page with caller-retained continuation keys.
use super::{report_error, with_addresses, AttemptError};
use std::{
    ffi::OsString,
    io::{self, Write},
    process::ExitCode,
};
use td_mta::{
    ids::{AccountId, BlobId, StoreEpoch},
    postgres_blob_scrub::{self, Key},
    postgres_input::Loaded,
    postgres_schema,
};
const COMMAND: &str = "postgres-blob-scrub-page";
struct Options {
    connection: String,
    epoch: StoreEpoch,
    page: postgres_blob_scrub::Options,
}
pub(super) fn parse_key(value: &str) -> Option<Key> {
    let (account, blob) = value.split_once(':')?;
    Some(Key {
        account: AccountId::parse(account).ok()?,
        blob: BlobId::parse(blob).ok()?,
    })
}
fn encode_key(key: Option<Key>) -> String {
    key.map(|k| format!("\"{}:{}\"", k.account, k.blob))
        .unwrap_or_else(|| "null".into())
}
fn arguments(mut args: impl Iterator<Item = OsString>) -> Option<Options> {
    let (mut connection, mut epoch, mut after, mut through, mut maximum) =
        (None, None, None, None, None);
    while let Some(flag) = args.next() {
        let value = args.next()?.into_string().ok()?;
        match flag.to_str()? {
            "--connection-file" if connection.is_none() => connection = Some(value),
            "--epoch" if epoch.is_none() => epoch = Some(StoreEpoch::parse(&value).ok()?),
            "--after" if after.is_none() => after = Some(parse_key(&value)?),
            "--through" if through.is_none() => through = Some(parse_key(&value)?),
            "--limit" if maximum.is_none() => {
                maximum = Some(
                    value
                        .parse::<usize>()
                        .ok()
                        .filter(|n| (1..=256).contains(n))?,
                )
            }
            _ => return None,
        }
    }
    if after.is_some_and(|after| through.is_none_or(|end| after > end)) {
        return None;
    }
    Some(Options {
        connection: connection?,
        epoch: epoch?,
        page: postgres_blob_scrub::Options {
            after,
            through,
            maximum: maximum.unwrap_or(128),
        },
    })
}
pub(super) fn run(args: impl Iterator<Item = OsString>) -> io::Result<ExitCode> {
    let Some(options) = arguments(args) else {
        report_error(COMMAND, "arguments", "invalid-arguments")?;
        return Ok(ExitCode::from(2));
    };
    let loaded = match Loaded::open(&options.connection, "blob-scrub") {
        Ok(loaded) => loaded,
        Err(error) => {
            report_error(COMMAND, "bootstrap", &error.to_string())?;
            return Ok(ExitCode::FAILURE);
        }
    };
    match with_addresses(&loaded, |request, deadline| {
        postgres_blob_scrub::inspect(
            request,
            &loaded.trust,
            deadline,
            options.epoch,
            options.page,
        )
        .map_err(classify)
    }) {
        Ok(report) => {
            let status = if report.failed == 0 { "ok" } else { "corrupt" };
            writeln!(io::stdout().lock(),"{{\"schema\":1,\"command\":\"{COMMAND}\",\"status\":\"{status}\",\"store_epoch\":\"{}\",\"scanned\":{},\"failed\":{},\"verified_bytes\":{},\"after\":{},\"through\":{},\"first_failure\":{},\"done\":{},\"semantic_verified\":false}}",options.epoch,report.scanned,report.failed,report.verified_bytes,encode_key(report.after),encode_key(report.through),encode_key(report.first_failure),report.done)?;
            Ok(if report.failed == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Err((stage, code)) => {
            report_error(COMMAND, stage, code)?;
            Ok(ExitCode::FAILURE)
        }
    }
}
pub(super) fn classify(error: postgres_blob_scrub::Error) -> AttemptError {
    use postgres_blob_scrub::Error;
    match error {
        Error::Connection(error) => AttemptError::from(postgres_schema::Error::Connection(error)),
        Error::Schema(error) => AttemptError::from(error),
        Error::Body(td_mta::postgres_body::Error::Read(td_mta::ports::Error::Deadline)) => {
            AttemptError::Failed("blob-scrub", "deadline")
        }
        Error::Body(_) => AttemptError::Failed("blob-scrub", "body-read-failed"),
        Error::Owner => AttemptError::Failed("blob-scrub", "owner-required"),
        Error::Invalid => AttemptError::Failed("blob-scrub", "invalid-state"),
        Error::Database => AttemptError::Failed("blob-scrub", "query-failed"),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn continuation_and_bounds_are_checked_before_opening_files() {
        let base = [
            "--connection-file",
            "/private/owner",
            "--epoch",
            "00000000000000000000000000000000",
        ];
        assert!(arguments(base.into_iter().map(Into::into)).is_some());
        let key = "01010101010101010101010101010101:03030303030303030303030303030303";
        assert!(arguments(
            base.into_iter()
                .chain(["--through", key, "--after", key])
                .map(Into::into)
        )
        .is_some());
        for extra in [
            vec!["--after", key],
            vec!["--through", "bad"],
            vec!["--limit", "0"],
            vec!["--limit", "257"],
            vec!["--limit", "1", "--limit", "1"],
            vec!["--repair", "yes"],
        ] {
            assert!(arguments(base.into_iter().chain(extra).map(Into::into)).is_none());
        }
        assert_eq!(
            parse_key(key).map(|key| encode_key(Some(key))),
            Some(format!("\"{key}\""))
        );
    }
}
