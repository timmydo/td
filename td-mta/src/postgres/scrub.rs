//! Scheduled scrub step and freshness check; each invocation is finite.
use super::{report_error, scrub_state::FileState, with_addresses};
use std::{
    ffi::OsString,
    io::{self, Write},
    process::ExitCode,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use td_mta::{ids::StoreEpoch, postgres_blob_scrub, postgres_input::Loaded};
struct Options {
    state: String,
    epoch: StoreEpoch,
    connection: Option<String>,
    maximum: usize,
}
fn arguments(mut args: impl Iterator<Item = OsString>, status: bool) -> Option<Options> {
    let (mut state, mut epoch, mut connection, mut maximum) = (None, None, None, None);
    while let Some(flag) = args.next() {
        let value = args.next()?.into_string().ok()?;
        match flag.to_str()? {
            "--state-dir" if state.is_none() => state = Some(value),
            "--epoch" if epoch.is_none() => epoch = Some(StoreEpoch::parse(&value).ok()?),
            "--connection-file" if !status && connection.is_none() => connection = Some(value),
            "--limit" if !status && maximum.is_none() => {
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
    if !status && connection.is_none() {
        return None;
    }
    Some(Options {
        state: state?,
        epoch: epoch?,
        connection,
        maximum: maximum.unwrap_or(128),
    })
}
pub(super) fn run(args: impl Iterator<Item = OsString>, status: bool) -> io::Result<ExitCode> {
    let command = if status {
        "postgres-blob-scrub-status"
    } else {
        "postgres-blob-scrub"
    };
    let Some(options) = arguments(args, status) else {
        report_error(command, "arguments", "invalid-arguments")?;
        return Ok(ExitCode::from(2));
    };
    let result = (|| {
        let file = FileState::open(
            &options.state,
            if status {
                Duration::from_secs(15)
            } else {
                Duration::ZERO
            },
        )?;
        let mut state = file.load(options.epoch)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "scrub-clock")?
            .as_secs();
        state.observe(now)?;
        if status {
            file.save(&state)?;
            let healthy = state.healthy(now, 172800);
            return Ok((state, healthy, "status"));
        }
        if !state.active && state.completed != 0 && now - state.completed < 86400 {
            file.save(&state)?;
            let healthy = state.healthy(now, 172800);
            return Ok((state, healthy, "waiting"));
        }
        if !state.active {
            state.begin(now);
        }
        file.save(&state)?;
        let result = (|| {
            let loaded = Loaded::open(
                options.connection.as_deref().ok_or("scrub-connection")?,
                "blob-scrub",
            )
            .map_err(|_| "scrub-connection")?;
            with_addresses(&loaded, |request, deadline| {
                postgres_blob_scrub::inspect(
                    request,
                    &loaded.trust,
                    deadline,
                    options.epoch,
                    postgres_blob_scrub::Options {
                        after: state.after,
                        through: state.through,
                        maximum: options.maximum,
                    },
                )
                .map_err(super::blob_scrub::classify)
            })
            .map_err(|(_, code)| code)
        })();
        match result {
            Ok(page) => {
                state.apply(page, now)?;
                file.save(&state)?;
                let good = state.failed == 0 && state.last_failed == 0;
                Ok((state, good, if page.done { "complete" } else { "progress" }))
            }
            Err(code) => {
                state.operation_failed = true;
                file.save(&state)?;
                Err(code)
            }
        }
    })();
    match result {
        Ok((state, healthy, phase)) => {
            let status = if healthy { "ok" } else { "attention" };
            writeln!(io::stdout().lock(),"{{\"schema\":1,\"command\":\"{command}\",\"status\":\"{status}\",\"phase\":\"{phase}\",\"progress\":{}}}",state.json())?;
            Ok(if healthy {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Err(code) => {
            report_error(command, "scrub", code)?;
            Ok(ExitCode::FAILURE)
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn status_requires_no_database_credentials() {
        let base = [
            "--state-dir",
            "/private/scrub",
            "--epoch",
            "00000000000000000000000000000000",
        ];
        assert!(arguments(base.into_iter().map(Into::into), true).is_some());
        assert!(arguments(base.into_iter().map(Into::into), false).is_none());
        assert!(arguments(
            base.into_iter()
                .chain(["--connection-file", "/private/owner"])
                .map(Into::into),
            true
        )
        .is_none());
    }
}
