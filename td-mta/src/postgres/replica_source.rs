//! Owner-only replica identity preparation; no network copy or promotion.
use super::{report_error, with_addresses, AttemptError};
use std::{
    ffi::{OsStr, OsString},
    io::{self, Write},
    process::ExitCode,
};
use td_mta::{
    ids::StoreEpoch,
    postgres_input::Loaded,
    postgres_replica_source::{self as replica, Error, Source},
    postgres_schema,
};

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Operation {
    Inspect,
    Initialize,
}
impl Operation {
    pub(super) fn parse(value: &OsStr) -> Option<Self> {
        match value.to_str()? {
            "replica-source-inspect" => Some(Self::Inspect),
            "replica-source-init" => Some(Self::Initialize),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Inspect => "postgres-replica-source-inspect",
            Self::Initialize => "postgres-replica-source-init",
        }
    }
}
struct Options {
    connection: String,
    epoch: StoreEpoch,
    source: Option<Source>,
}
fn arguments(operation: Operation, mut args: impl Iterator<Item = OsString>) -> Option<Options> {
    let (mut connection, mut epoch, mut origin, mut source_epoch) = (None, None, None, None);
    while let Some(flag) = args.next() {
        let value = args.next()?.into_string().ok()?;
        match flag.to_str()? {
            "--connection-file" if connection.is_none() => connection = Some(value),
            "--epoch" if epoch.is_none() => epoch = Some(StoreEpoch::parse(&value).ok()?),
            "--source-origin" if operation == Operation::Initialize && origin.is_none() => {
                origin = Some(value)
            }
            "--source-epoch" if operation == Operation::Initialize && source_epoch.is_none() => {
                source_epoch = Some(StoreEpoch::parse(&value).ok()?)
            }
            _ => return None,
        }
    }
    Some(Options {
        connection: connection?,
        epoch: epoch?,
        source: match operation {
            Operation::Inspect => None,
            Operation::Initialize => Some(Source::new(&origin?, source_epoch?).ok()?),
        },
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
    let loaded = match Loaded::open(&options.connection, "replica-source") {
        Ok(loaded) => loaded,
        Err(error) => {
            report_error(name, "bootstrap", &error.to_string())?;
            return Ok(ExitCode::FAILURE);
        }
    };
    let result = with_addresses(&loaded, |request, deadline| {
        if let Some(source) = options.source.as_ref() {
            replica::initialize(request, &loaded.trust, deadline, options.epoch, source)
                .map_err(classify)?;
            return Ok(None);
        }
        replica::inspect(request, &loaded.trust, deadline, options.epoch).map_err(classify)
    });
    match result {
        Ok(source) => {
            // Validated origins contain no credentials, quotes or escapes.
            let source_json = match source.as_ref().or(options.source.as_ref()) {
                Some(source) => format!(
                    "{{\"origin\":\"{}\",\"epoch\":\"{}\",\"protocol\":1}}",
                    source.origin(),
                    source.epoch()
                ),
                None => "null".to_owned(),
            };
            writeln!(io::stdout().lock(), "{{\"schema\":1,\"command\":\"{name}\",\"status\":\"ok\",\"store_epoch\":\"{}\",\"source\":{source_json},\"copy_complete\":false,\"recovery_verified\":false}}", options.epoch)?;
            Ok(ExitCode::SUCCESS)
        }
        Err((stage, code)) => {
            report_error(name, stage, code)?;
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
        Error::Invalid => "invalid-source",
        Error::Capacity => "capacity",
        Error::Occupied => "database-not-empty",
        Error::Conflict => "source-conflict",
        Error::Integrity => "source-integrity",
        Error::Database => "query-failed",
        Error::Indeterminate => "indeterminate-inspect-replica-source",
    };
    AttemptError::Failed("replica-source", code)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn initialization_requires_exact_source_and_store_scope() {
        let base = [
            "--connection-file",
            "private",
            "--epoch",
            "11111111111111111111111111111111",
        ];
        let source = [
            "--source-origin",
            "https://mail.example.test",
            "--source-epoch",
            "22222222222222222222222222222222",
        ];
        let parse =
            |op, extra: &[&str]| arguments(op, base.iter().chain(extra).map(OsString::from));
        assert!(parse(Operation::Inspect, &[]).is_some());
        assert!(parse(Operation::Initialize, &source).is_some());
        assert!(parse(Operation::Initialize, &[]).is_none());
        assert!(parse(Operation::Inspect, &source).is_none());
        for extra in [
            vec!["--force"],
            vec![
                "--source-origin",
                "http://mail.example.test",
                "--source-epoch",
                "22222222222222222222222222222222",
            ],
            vec!["--epoch", "11111111111111111111111111111111"],
            vec!["--source-origin", "https://mail.example.test"],
        ] {
            assert!(parse(Operation::Initialize, &extra).is_none());
        }
    }
}
