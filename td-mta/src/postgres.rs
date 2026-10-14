//! Explicit database preparation; never opens mail service admission.
use std::{
    ffi::{OsStr, OsString},
    io::{self, Write},
    process::ExitCode,
    time::{Duration, Instant},
};
use td_mta::{postgres_input::Loaded, postgres_probe, postgres_resolver, postgres_schema};

mod material;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Operation {
    Probe,
    Initialize,
    Upgrade,
    Inspect,
}
impl Operation {
    fn parse(value: &OsStr) -> Option<Self> {
        match value.to_str()? {
            "probe" => Some(Self::Probe),
            "schema-init" => Some(Self::Initialize),
            "schema-upgrade" => Some(Self::Upgrade),
            "schema-inspect" => Some(Self::Inspect),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Probe => "postgres-probe",
            Self::Initialize => "postgres-schema-init",
            Self::Upgrade => "postgres-schema-upgrade",
            Self::Inspect => "postgres-schema-inspect",
        }
    }
}

struct Options {
    connection: String,
    profile: String,
}

fn arguments(operation: Operation, mut args: impl Iterator<Item = OsString>) -> Option<Options> {
    let mut connection = None;
    let mut profile = None;
    while let Some(flag) = args.next() {
        match flag.to_str()? {
            "--connection-file" if connection.is_none() => {
                connection = Some(args.next()?.into_string().ok()?);
            }
            "--profile" if operation == Operation::Probe && profile.is_none() => {
                profile = Some(args.next()?.into_string().ok()?)
            }
            _ => return None,
        }
    }
    Some(Options {
        connection: connection?,
        profile: match operation {
            Operation::Probe => profile?,
            // Schema operations have no profile selector. This value is only
            // the bootstrap parser's syntactic placeholder, never a DB lookup.
            _ => "schema".to_owned(),
        },
    })
}

pub fn run(mut args: impl Iterator<Item = OsString>) -> io::Result<ExitCode> {
    let command = args.next();
    if let Some(operation) = command.as_deref().and_then(material::Operation::parse) {
        return material::run(operation, args);
    }
    let operation = command.as_deref().and_then(Operation::parse);
    let Some(operation) = operation else {
        report_error("postgres", "arguments", "invalid-arguments")?;
        return Ok(ExitCode::from(2));
    };
    let name = operation.name();
    let Some(options) = arguments(operation, args) else {
        report_error(name, "arguments", "invalid-arguments")?;
        return Ok(ExitCode::from(2));
    };
    let loaded = match Loaded::open(&options.connection, &options.profile) {
        Ok(loaded) => loaded,
        Err(error) => {
            report_error(name, "bootstrap", &error.to_string())?;
            return Ok(ExitCode::FAILURE);
        }
    };
    match check(&loaded, operation) {
        Ok(Outcome::Probe(status)) => {
            writeln!(io::stdout().lock(), "{{\"schema\":1,\"command\":\"{name}\",\"status\":\"ok\",\"server_version\":{},\"schema_checked\":false,\"profile_checked\":false}}", status.server_version)?;
            Ok(ExitCode::SUCCESS)
        }
        Ok(Outcome::Schema(receipt)) => {
            let encoded = receipt.store_epoch;
            writeln!(io::stdout().lock(), "{{\"schema\":1,\"command\":\"{name}\",\"status\":\"ok\",\"schema_version\":{},\"store_epoch\":\"{encoded}\",\"ledger_checked\":true,\"serving_schema_checked\":false,\"profile_checked\":false}}", receipt.version)?;
            Ok(ExitCode::SUCCESS)
        }
        Err((stage, code)) => {
            report_error(name, stage, code)?;
            Ok(ExitCode::FAILURE)
        }
    }
}

fn report_error(command: &str, stage: &str, code: &str) -> io::Result<()> {
    writeln!(io::stdout().lock(), "{{\"schema\":1,\"command\":\"{command}\",\"status\":\"error\",\"stage\":\"{stage}\",\"error\":\"{code}\"}}")
}

enum Outcome {
    Probe(postgres_probe::Status),
    Schema(postgres_schema::Receipt),
}

fn check(loaded: &Loaded, operation: Operation) -> Result<Outcome, (&'static str, &'static str)> {
    with_addresses(loaded, |request, deadline| {
        match operation {
            Operation::Probe => postgres_probe::probe(request, &loaded.trust, deadline)
                .map(Outcome::Probe)
                .map_err(postgres_schema::Error::Connection),
            Operation::Initialize => {
                postgres_schema::initialize(request, &loaded.trust, deadline).map(Outcome::Schema)
            }
            Operation::Upgrade => {
                postgres_schema::upgrade(request, &loaded.trust, deadline).map(Outcome::Schema)
            }
            Operation::Inspect => {
                postgres_schema::inspect(request, &loaded.trust, deadline).map(Outcome::Schema)
            }
        }
        .map_err(AttemptError::from)
    })
}

enum AttemptError {
    Transport,
    Failed(&'static str, &'static str),
}
impl From<postgres_schema::Error> for AttemptError {
    fn from(error: postgres_schema::Error) -> Self {
        if error == postgres_schema::Error::Connection(postgres_probe::Error::Transport) {
            Self::Transport
        } else {
            let (stage, code) = classify(error);
            Self::Failed(stage, code)
        }
    }
}

fn with_addresses<T>(
    loaded: &Loaded,
    mut operation: impl FnMut(&postgres_probe::Request<'_>, Instant) -> Result<T, AttemptError>,
) -> Result<T, (&'static str, &'static str)> {
    let executable = std::env::current_exe().map_err(|_| ("resolver", "executable"))?;
    // Cold files precede one shared DNS/connection/operation deadline.
    let now = Instant::now();
    let deadline = now + Duration::from_secs(10);
    let addresses =
        postgres_resolver::resolve(&executable, &loaded.bootstrap, now + Duration::from_secs(5))
            .map_err(|error| {
                (
                    "resolver",
                    match error {
                        postgres_resolver::Error::Deadline => "deadline",
                        _ => "resolution-failed",
                    },
                )
            })?;
    for address in addresses {
        match operation(&loaded.bootstrap.request(address), deadline) {
            Ok(value) => return Ok(value),
            Err(AttemptError::Transport) => continue,
            Err(AttemptError::Failed(stage, code)) => return Err((stage, code)),
        }
    }
    Err(exhausted(deadline))
}

fn classify(error: postgres_schema::Error) -> (&'static str, &'static str) {
    use postgres_schema::Error;
    match error {
        Error::Connection(error) => (
            "database",
            match error {
                postgres_probe::Error::Invalid => "invalid-request",
                postgres_probe::Error::Deadline => "deadline",
                postgres_probe::Error::Tls => "tls",
                postgres_probe::Error::Database => "connection-or-authentication",
                postgres_probe::Error::Settings => "primary-or-durability-settings",
                postgres_probe::Error::Transport => "transport",
            },
        ),
        other => (
            "schema",
            match other {
                Error::Version => "unsupported-version-or-checksum",
                Error::Busy => "migration-busy",
                Error::Exists => "namespace-exists",
                Error::Invalid => "invalid-ledger-or-privileges",
                Error::Database => "schema-operation-failed",
                Error::Crypto => "crypto",
                Error::Indeterminate => "indeterminate-do-not-retry",
                Error::Connection(_) => "connection-or-authentication",
            },
        ),
    }
}

fn exhausted(deadline: Instant) -> (&'static str, &'static str) {
    (
        "database",
        if Instant::now() >= deadline {
            "deadline"
        } else {
            "transport"
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(args: &[&str]) -> Option<Options> {
        let mut args = args.iter().map(OsString::from);
        let operation = Operation::parse(&args.next()?)?;
        arguments(operation, args)
    }
    #[test]
    fn schema_operations_do_not_take_a_profile() {
        for operation in ["schema-init", "schema-inspect", "schema-upgrade"] {
            assert!(parse(&[operation, "--connection-file", "/private/db"]).is_some());
            assert!(parse(&[
                operation,
                "--connection-file",
                "/private/db",
                "--profile",
                "x"
            ])
            .is_none());
        }
        assert_eq!(
            classify(postgres_schema::Error::Indeterminate),
            ("schema", "indeterminate-do-not-retry")
        );
    }
    #[test]
    fn last_address_exhaustion_distinguishes_deadline_from_transport() {
        assert_eq!(exhausted(Instant::now()), ("database", "deadline"));
        assert_eq!(
            exhausted(Instant::now() + Duration::from_secs(60)),
            ("database", "transport")
        );
    }
    #[test]
    fn command_requires_one_protected_file_and_profile() {
        assert!(parse(&[
            "probe",
            "--connection-file",
            "/private/db",
            "--profile",
            "desktop"
        ])
        .is_some());
        assert!(parse(&[
            "probe",
            "--profile",
            "desktop",
            "--connection-file",
            "/private/db"
        ])
        .is_some());
        for args in [
            vec![],
            vec!["check"],
            vec!["probe"],
            vec!["probe", "--connection-file", "/private/db"],
            vec!["probe", "--profile", "desktop"],
            vec!["probe", "--connection", "secret", "--profile", "desktop"],
            vec![
                "probe",
                "--connection-file",
                "/a",
                "--profile",
                "desktop",
                "--profile",
                "other",
            ],
            vec![
                "probe",
                "--connection-file",
                "/a",
                "--connection-file",
                "/b",
                "--profile",
                "desktop",
            ],
        ] {
            assert!(parse(&args).is_none());
        }
    }
}
