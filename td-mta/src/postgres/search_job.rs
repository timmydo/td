//! Finite scheduled preparation with an explicit uncertainty latch.
use super::{
    report_error, scrub_state::FileState, search_job_state::State, with_addresses, AttemptError,
};
use std::{
    ffi::OsString,
    io::{self, Write},
    process::ExitCode,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use td_mta::{ids::StoreEpoch, postgres_input::Loaded, postgres_schema, postgres_search_prepare};

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Operation {
    Step,
    Status,
    Resume,
}
impl Operation {
    fn name(self) -> &'static str {
        match self {
            Self::Step => "postgres-search-prepare",
            Self::Status => "postgres-search-prepare-status",
            Self::Resume => "postgres-search-prepare-resume",
        }
    }
}
struct Options {
    state: String,
    epoch: StoreEpoch,
    connection: Option<String>,
}
fn arguments(mut args: impl Iterator<Item = OsString>, operation: Operation) -> Option<Options> {
    let (mut state, mut epoch, mut connection) = (None, None, None);
    while let Some(flag) = args.next() {
        let value = args.next()?.into_string().ok()?;
        match flag.to_str()? {
            "--state-dir" if state.is_none() => state = Some(value),
            "--epoch" if epoch.is_none() => epoch = Some(StoreEpoch::parse(&value).ok()?),
            "--connection-file" if operation != Operation::Status && connection.is_none() => {
                connection = Some(value)
            }
            _ => return None,
        }
    }
    if operation != Operation::Status && connection.is_none() {
        return None;
    }
    Some(Options {
        state: state?,
        epoch: epoch?,
        connection,
    })
}
fn now() -> Result<u64, &'static str> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|v| v.as_secs())
        .map_err(|_| "search-clock")
}
fn save(file: &FileState, state: &State) -> Result<(), &'static str> {
    file.save_text(&state.json().to_string())
}
pub(super) fn run(
    args: impl Iterator<Item = OsString>,
    operation: Operation,
) -> io::Result<ExitCode> {
    let command = operation.name();
    let Some(options) = arguments(args, operation) else {
        report_error(command, "arguments", "invalid-arguments")?;
        return Ok(ExitCode::from(2));
    };
    let result = (|| {
        let file = FileState::open(&options.state, Duration::from_secs(15))?;
        let mut state = match file.load_text()? {
            Some(text) => State::parse(&text, options.epoch)?,
            None => State::new(options.epoch),
        };
        let time = now()?;
        state.observe(time)?;
        if operation == Operation::Status {
            save(&file, &state)?;
            let healthy = state.healthy(time);
            return Ok((state, healthy, "status"));
        }
        if state.in_flight && operation != Operation::Resume {
            return Err("search-commit-unknown");
        }
        let loaded = match Loaded::open(
            options.connection.as_deref().ok_or("search-connection")?,
            "search-prepare",
        ) {
            Ok(loaded) => loaded,
            Err(_) => {
                state.operation_failed = true;
                save(&file, &state)?;
                return Err("search-connection");
            }
        };
        if operation == Operation::Resume {
            if !state.in_flight {
                return Err("search-no-uncertainty");
            }
            // Replaying derived writes is safe, but never claims prior rollback.
            with_addresses(&loaded, |request, deadline| {
                let receipt = postgres_schema::inspect(request, &loaded.trust, deadline)
                    .map_err(AttemptError::from)?;
                if receipt.store_epoch != options.epoch {
                    return Err(AttemptError::Failed("search-prepare", "epoch-mismatch"));
                }
                Ok(())
            })
            .map_err(|(_, code)| code)?;
            state.in_flight = false;
            state.operation_failed = true;
            state.observe(now()?)?;
            save(&file, &state)?;
            return Ok((state, true, "replay-authorized"));
        }
        if !state.active && state.completed != 0 && time - state.completed < 300 {
            save(&file, &state)?;
            let healthy = state.healthy(time);
            return Ok((state, healthy, "waiting"));
        }
        if !state.active {
            state.begin(time)?;
        }
        state.in_flight = true;
        save(&file, &state)?;
        let result = with_addresses(&loaded, |request, deadline| {
            postgres_search_prepare::run(
                request,
                &loaded.trust,
                deadline,
                options.epoch,
                postgres_search_prepare::Options {
                    after: state.after,
                    through: state.through,
                    maximum: 16,
                },
            )
            .map_err(super::search_prepare::classify)
        });
        match result {
            Ok(page) => {
                state.apply(page, now()?)?;
                save(&file, &state)?;
                Ok((state, true, if page.done { "complete" } else { "progress" }))
            }
            Err((_, code)) => {
                state.in_flight = code == "commit-unknown";
                state.operation_failed = true;
                state.observe(now()?)?;
                save(&file, &state)?;
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
            report_error(command, "search-prepare", code)?;
            Ok(ExitCode::FAILURE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_status_accepts_no_credentials() {
        let base = [
            "--state-dir",
            "/private/search",
            "--epoch",
            "00000000000000000000000000000000",
        ];
        assert!(arguments(base.into_iter().map(Into::into), Operation::Status).is_some());
        for operation in [Operation::Step, Operation::Resume] {
            assert!(arguments(base.into_iter().map(Into::into), operation).is_none());
            assert!(arguments(
                base.into_iter()
                    .chain(["--connection-file", "/private/owner"])
                    .map(Into::into),
                operation
            )
            .is_some());
        }
        assert!(arguments(
            base.into_iter()
                .chain(["--connection-file", "/private/owner"])
                .map(Into::into),
            Operation::Status
        )
        .is_none());
    }
}
