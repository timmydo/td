//! Explicit owner maintenance observation and monotone entry.
use super::{report_error, with_addresses, AttemptError};
use std::{
    ffi::{OsStr, OsString},
    io::{self, Write},
    process::ExitCode,
};
use td_mta::{
    ids::StoreEpoch,
    postgres_input::Loaded,
    postgres_maintenance::{self as maintenance, Error, ExternalFence},
    postgres_schema,
};

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Operation {
    Inspect,
    Enter,
}
impl Operation {
    pub(super) fn parse(value: &OsStr) -> Option<Self> {
        match value.to_str()? {
            "maintenance-inspect" => Some(Self::Inspect),
            "maintenance-enter" => Some(Self::Enter),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Inspect => "postgres-maintenance-inspect",
            Self::Enter => "postgres-maintenance-enter",
        }
    }
}
struct Options {
    connection: String,
    epoch: StoreEpoch,
}
fn arguments(operation: Operation, mut args: impl Iterator<Item = OsString>) -> Option<Options> {
    let (mut connection, mut epoch, mut fenced) = (None, None, false);
    while let Some(flag) = args.next() {
        match flag.to_str()? {
            "--connection-file" if connection.is_none() => {
                connection = Some(args.next()?.into_string().ok()?)
            }
            "--epoch" if epoch.is_none() => {
                epoch = Some(StoreEpoch::parse(&args.next()?.into_string().ok()?).ok()?)
            }
            "--instances-and-smtp-fenced" if operation == Operation::Enter && !fenced => {
                fenced = true
            }
            _ => return None,
        }
    }
    if operation == Operation::Enter && !fenced {
        return None;
    }
    Some(Options {
        connection: connection?,
        epoch: epoch?,
    })
}
pub(super) fn run(
    operation: Operation,
    args: impl Iterator<Item = OsString>,
) -> io::Result<ExitCode> {
    let name = operation.name();
    let Some(options) = arguments(operation, args) else {
        report_error(name, "arguments", "invalid-arguments")?;
        return Ok(ExitCode::from(2));
    };
    let loaded = match Loaded::open(&options.connection, "maintenance") {
        Ok(loaded) => loaded,
        Err(error) => {
            report_error(name, "bootstrap", &error.to_string())?;
            return Ok(ExitCode::FAILURE);
        }
    };
    let result = with_addresses(&loaded, |request, deadline| {
        match operation {
            Operation::Inspect => {
                maintenance::inspect(request, &loaded.trust, deadline, options.epoch)
            }
            Operation::Enter => maintenance::enter(
                request,
                &loaded.trust,
                deadline,
                options.epoch,
                ExternalFence::InstancesAndSmtpEgressStopped,
            )
            .map(|()| true),
        }
        .map_err(classify)
    });
    match result {
        Ok(blocked) => {
            writeln!(io::stdout().lock(),"{{\"schema\":1,\"command\":\"{name}\",\"status\":\"ok\",\"store_epoch\":\"{}\",\"blocked\":{blocked},\"recovery_verified\":false}}",options.epoch)?;
            Ok(ExitCode::SUCCESS)
        }
        Err((stage, code)) => {
            writeln!(io::stdout().lock(),"{{\"schema\":1,\"command\":\"{name}\",\"status\":\"error\",\"stage\":\"{stage}\",\"error\":\"{code}\",\"store_epoch\":\"{}\"}}",options.epoch)?;
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
        Error::Owner => "owner-required",
        Error::Epoch => "epoch-mismatch",
        Error::Integrity => "barrier-integrity",
        Error::Database => "query-failed",
        Error::Indeterminate => "indeterminate-inspect-maintenance",
    };
    AttemptError::Failed("maintenance", code)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn entry_requires_one_explicit_fence_attestation() {
        let base = [
            "--connection-file",
            "connection",
            "--epoch",
            "11111111111111111111111111111111",
        ];
        let parse = |op, extra: &[&str]| {
            arguments(
                op,
                base.iter()
                    .chain(extra.iter())
                    .map(|value| OsString::from(*value)),
            )
        };
        assert!(parse(Operation::Inspect, &[]).is_some());
        assert!(parse(Operation::Enter, &[]).is_none());
        assert!(parse(Operation::Enter, &["--instances-and-smtp-fenced"]).is_some());
        for extra in [
            vec!["--instances-and-smtp-fenced", "--instances-and-smtp-fenced"],
            vec!["--instances-and-smtp-fenced", "true"],
            vec!["--force"],
            vec!["--epoch", "22222222222222222222222222222222"],
            vec!["--connection-file", "again"],
            vec!["--unknown"],
        ] {
            assert!(parse(Operation::Enter, &extra).is_none());
        }
        assert!(parse(Operation::Inspect, &["--instances-and-smtp-fenced"]).is_none());
        assert!(arguments(
            Operation::Inspect,
            ["--connection-file", "connection"]
                .into_iter()
                .map(OsString::from)
        )
        .is_none());
        assert!(arguments(
            Operation::Inspect,
            ["--epoch", "bad"].into_iter().map(OsString::from)
        )
        .is_none());
    }
}
