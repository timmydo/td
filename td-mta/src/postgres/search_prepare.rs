//! Explicit finite missing-search page, with caller-retained continuation keys.
use super::{report_error, with_addresses, AttemptError};
use std::{
    ffi::OsString,
    io::{self, Write},
    process::ExitCode,
};
use td_mta::{
    ids::StoreEpoch,
    postgres_input::Loaded,
    postgres_schema,
    postgres_search_prepare::{self, Key},
};

const COMMAND: &str = "postgres-search-prepare-page";
struct Options {
    connection: String,
    epoch: StoreEpoch,
    page: postgres_search_prepare::Options,
}
fn arguments(mut args: impl Iterator<Item = OsString>) -> Option<Options> {
    let (mut connection, mut epoch, mut after, mut through, mut maximum) =
        (None, None, None, None, None);
    while let Some(flag) = args.next() {
        let value = args.next()?.into_string().ok()?;
        match flag.to_str()? {
            "--connection-file" if connection.is_none() => connection = Some(value),
            "--epoch" if epoch.is_none() => epoch = Some(StoreEpoch::parse(&value).ok()?),
            "--after" if after.is_none() => after = Some(super::blob_scrub::parse_key(&value)?),
            "--through" if through.is_none() => {
                through = Some(super::blob_scrub::parse_key(&value)?)
            }
            "--limit" if maximum.is_none() => {
                maximum = Some(
                    value
                        .parse::<usize>()
                        .ok()
                        .filter(|n| (1..=postgres_search_prepare::MAXIMUM).contains(n))?,
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
        page: postgres_search_prepare::Options {
            after,
            through,
            maximum: maximum.unwrap_or(postgres_search_prepare::MAXIMUM),
        },
    })
}
fn encode_key(key: Option<Key>) -> String {
    key.map(|key| format!("\"{}:{}\"", key.account, key.blob))
        .unwrap_or_else(|| "null".into())
}
pub(super) fn classify(error: postgres_search_prepare::Error) -> AttemptError {
    use postgres_search_prepare::Error;
    let code = match error {
        Error::Connection(error) => {
            return AttemptError::from(postgres_schema::Error::Connection(error))
        }
        Error::Schema(error) => return AttemptError::from(error),
        Error::Body(td_mta::postgres_body::Error::Read(td_mta::ports::Error::Deadline)) => {
            "preparation-deadline"
        }
        Error::Body(td_mta::postgres_body::Error::Read(td_mta::ports::Error::Capacity)) => {
            "preparation-capacity"
        }
        Error::Body(td_mta::postgres_body::Error::Read(td_mta::ports::Error::Crypto)) => {
            "preparation-crypto"
        }
        Error::Body(_) => "body-check-failed",
        Error::Owner => "owner-required",
        Error::Invalid => "invalid-state",
        Error::Database => "query-failed",
        Error::Indeterminate => "commit-unknown",
    };
    AttemptError::Failed("search-prepare", code)
}
pub(super) fn run(args: impl Iterator<Item = OsString>) -> io::Result<ExitCode> {
    let Some(options) = arguments(args) else {
        report_error(COMMAND, "arguments", "invalid-arguments")?;
        return Ok(ExitCode::from(2));
    };
    let loaded = match Loaded::open(&options.connection, "search-prepare") {
        Ok(loaded) => loaded,
        Err(error) => {
            report_error(COMMAND, "bootstrap", &error.to_string())?;
            return Ok(ExitCode::FAILURE);
        }
    };
    match with_addresses(&loaded, |request, deadline| {
        postgres_search_prepare::run(
            request,
            &loaded.trust,
            deadline,
            options.epoch,
            options.page,
        )
        .map_err(classify)
    }) {
        Ok(report) => {
            writeln!(io::stdout().lock(),"{{\"schema\":1,\"command\":\"{COMMAND}\",\"status\":\"ok\",\"store_epoch\":\"{}\",\"search_version\":{},\"scanned\":{},\"written\":{},\"skipped\":{},\"verified_bytes\":{},\"after\":{},\"through\":{},\"done\":{}}}",options.epoch,postgres_search_prepare::VERSION,report.scanned,report.written,report.skipped,report.verified_bytes,encode_key(report.after),encode_key(report.through),report.done)?;
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
    fn continuation_requires_a_valid_upper_bound_and_bounded_page() {
        let base = [
            "--connection-file",
            "/private/owner",
            "--epoch",
            "00000000000000000000000000000000",
        ];
        let key = "00000000000000000000000000000001:00000000000000000000000000000001";
        assert!(arguments(base.into_iter().map(Into::into)).is_some());
        assert!(arguments(base.into_iter().chain(["--after", key]).map(Into::into)).is_none());
        assert!(arguments(
            base.into_iter()
                .chain(["--after", key, "--through", key])
                .map(Into::into)
        )
        .is_some());
        for limit in ["0", "17", "18446744073709551616"] {
            assert!(
                arguments(base.into_iter().chain(["--limit", limit]).map(Into::into)).is_none()
            );
        }
        assert!(arguments(
            base.into_iter()
                .chain(["--through", key, "--through", key])
                .map(Into::into)
        )
        .is_none());
    }
}
