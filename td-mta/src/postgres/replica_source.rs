//! Owner-only replica identity preparation; no network copy or promotion.
use super::{report_error, with_connection, AttemptError};
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
    Grants,
}
impl Operation {
    pub(super) fn parse(value: &OsStr) -> Option<Self> {
        match value.to_str()? {
            "replica-source-inspect" => Some(Self::Inspect),
            "replica-source-init" => Some(Self::Initialize),
            "replica-grants" => Some(Self::Grants),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Inspect => "postgres-replica-source-inspect",
            Self::Initialize => "postgres-replica-source-init",
            Self::Grants => "postgres-replica-grants",
        }
    }
}
struct Options {
    connection: String,
    epoch: StoreEpoch,
    source: Option<Source>,
    /// `replica-grants`: the publisher's and the reader's connection files.
    logins: Option<(String, String)>,
}
fn arguments(operation: Operation, mut args: impl Iterator<Item = OsString>) -> Option<Options> {
    let (mut connection, mut epoch, mut origin, mut source_epoch) = (None, None, None, None);
    let (mut publisher, mut reader) = (None, None);
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
            "--publisher-connection-file"
                if operation == Operation::Grants && publisher.is_none() =>
            {
                publisher = Some(value)
            }
            "--reader-connection-file" if operation == Operation::Grants && reader.is_none() => {
                reader = Some(value)
            }
            _ => return None,
        }
    }
    Some(Options {
        connection: connection?,
        epoch: epoch?,
        source: match operation {
            Operation::Inspect | Operation::Grants => None,
            Operation::Initialize => Some(Source::new(&origin?, source_epoch?).ok()?),
        },
        logins: match operation {
            Operation::Grants => Some((publisher?, reader?)),
            _ => None,
        },
    })
}

/// `replica-grants`: the owner's, publisher's and reader's files, each
/// loaded cold before any connection.
fn grants(name: &str, options: &Options, logins: &(String, String)) -> io::Result<ExitCode> {
    let mut loaded = [None, None, None];
    for ((slot, file), stage) in loaded
        .iter_mut()
        .zip([&options.connection, &logins.0, &logins.1])
        .zip(["owner-bootstrap", "publisher-bootstrap", "reader-bootstrap"])
    {
        match Loaded::open(file, "replica-grants") {
            Ok(file) => *slot = Some(file),
            Err(error) => {
                report_error(name, stage, error.code())?;
                return Ok(ExitCode::FAILURE);
            }
        }
    }
    let [Some(owner), Some(publisher), Some(reader)] = &loaded else {
        report_error(name, "bootstrap", "missing-connection")?;
        return Ok(ExitCode::FAILURE);
    };
    let result = with_connection(owner, |request, deadline| {
        replica::grant(
            request,
            &publisher.bootstrap.request(),
            &reader.bootstrap.request(),
            deadline,
            options.epoch,
        )
        .map_err(grant_error)
    });
    match result {
        Ok(()) => {
            writeln!(io::stdout().lock(), "{{\"schema\":1,\"command\":\"{name}\",\"status\":\"ok\",\"store_epoch\":\"{}\",\"publisher_qualified\":true,\"reader_qualified\":true}}", options.epoch)?;
            Ok(ExitCode::SUCCESS)
        }
        Err((stage, code)) => {
            report_error(name, stage, code)?;
            Ok(ExitCode::FAILURE)
        }
    }
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
    if let Some(logins) = &options.logins {
        return grants(name, &options, logins);
    }
    let loaded = match Loaded::open(&options.connection, "replica-source") {
        Ok(loaded) => loaded,
        Err(error) => {
            report_error(name, "bootstrap", &error.to_string())?;
            return Ok(ExitCode::FAILURE);
        }
    };
    let result = with_connection(&loaded, |request, deadline| {
        if let Some(source) = options.source.as_ref() {
            replica::initialize(request, deadline, options.epoch, source).map_err(classify)?;
            return Ok(None);
        }
        replica::inspect(request, deadline, options.epoch).map_err(classify)
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
        Error::Unbound => "replica-unbound",
        Error::Logins => "invalid-replica-logins",
        Error::Publisher(_) | Error::Reader(_) => return grant_error(error),
    };
    AttemptError::Failed("replica-source", code)
}

/// `replica-grants`' own outcomes: a refusal after the grants names the
/// login and its cause in the `qualification` stage.
fn grant_error(error: Error) -> AttemptError {
    use td_mta::postgres_runtime_access::Error as Access;
    let code = match error {
        Error::Indeterminate => "indeterminate-rerun-replica-grants",
        Error::Publisher(Access::Role) => "publisher-runtime-owner-or-privileged-role",
        Error::Publisher(Access::Privileges) => "publisher-runtime-data-privileges",
        Error::Publisher(Access::Maintenance) => "publisher-maintenance-barrier",
        Error::Publisher(Access::Replica) => "publisher-replica-binding",
        Error::Publisher(_) => "publisher-query-failed",
        Error::Reader(Access::Role) => "reader-runtime-owner-or-privileged-role",
        Error::Reader(Access::Privileges) => "reader-runtime-data-privileges",
        Error::Reader(Access::Maintenance) => "reader-maintenance-barrier",
        Error::Reader(Access::Replica) => "reader-replica-binding",
        Error::Reader(_) => "reader-query-failed",
        other => return classify(other),
    };
    let stage = if matches!(error, Error::Indeterminate) {
        "replica-source"
    } else {
        "qualification"
    };
    AttemptError::Failed(stage, code)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[allow(clippy::unwrap_used)]
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
        let logins = [
            "--publisher-connection-file",
            "runtime",
            "--reader-connection-file",
            "reader",
        ];
        let grants = parse(Operation::Grants, &logins).unwrap();
        assert_eq!(
            grants.logins,
            Some(("runtime".to_owned(), "reader".to_owned()))
        );
        assert!(grants.source.is_none());
        assert!(parse(Operation::Grants, &[]).is_none());
        assert!(parse(Operation::Grants, &logins[..2]).is_none());
        assert!(parse(Operation::Grants, &[&logins[..], &source[..]].concat()).is_none());
        assert!(parse(Operation::Inspect, &logins).is_none());
        assert!(parse(Operation::Initialize, &[&source[..], &logins[..]].concat()).is_none());
    }
}
