//! Explicit database administration; never starts a mail service.
use std::{
    ffi::{OsStr, OsString},
    io::{self, Write},
    process::ExitCode,
    time::{Duration, Instant},
};
use td_mta::{postgres_input::Loaded, postgres_probe, postgres_schema};

mod account;
mod account_admin;
mod audit;
mod blob_audit;
mod blob_scrub;
mod capacity;
mod configuration;
mod configuration_converge;
mod configuration_snapshot;
mod device_create;
mod device_list;
mod device_revoke;
mod logs;
mod maintenance;
mod material;
mod metadata_audit;
mod recovery;
mod replica;
mod replica_source;
mod scrub;
mod scrub_state;
mod search_job;
mod search_job_state;
mod search_prepare;
mod selection;
mod summary_backfill;
mod usage;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Operation {
    Probe,
    Initialize,
    Plan,
    Upgrade,
    Inspect,
    RuntimeCheck,
    ServiceProfileCheck,
}
impl Operation {
    fn parse(value: &OsStr) -> Option<Self> {
        match value.to_str()? {
            "probe" => Some(Self::Probe),
            "schema-plan" => Some(Self::Plan),
            "schema-init" => Some(Self::Initialize),
            "schema-upgrade" => Some(Self::Upgrade),
            "schema-inspect" => Some(Self::Inspect),
            "runtime-check" => Some(Self::RuntimeCheck),
            "service-profile-check" => Some(Self::ServiceProfileCheck),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Probe => "postgres-probe",
            Self::Plan => "postgres-schema-plan",
            Self::Initialize => "postgres-schema-init",
            Self::Upgrade => "postgres-schema-upgrade",
            Self::Inspect => "postgres-schema-inspect",
            Self::RuntimeCheck => "postgres-runtime-check",
            Self::ServiceProfileCheck => "postgres-service-profile-check",
        }
    }
}

struct Options {
    timeout: Duration,
    connection: String,
    profile: String,
}

fn arguments(operation: Operation, mut args: impl Iterator<Item = OsString>) -> Option<Options> {
    let mut connection = None;
    let mut profile = None;
    let mut timeout = None;
    while let Some(flag) = args.next() {
        match flag.to_str()? {
            "--connection-file" if connection.is_none() => {
                connection = Some(args.next()?.into_string().ok()?);
            }
            "--timeout-seconds" if operation == Operation::Upgrade && timeout.is_none() => {
                let seconds: u64 = args.next()?.to_str()?.parse().ok()?;
                let duration = Duration::from_secs(seconds);
                if duration.is_zero() || duration > postgres_schema::MAX_UPGRADE_DURATION {
                    return None;
                }
                timeout = Some(duration);
            }
            "--profile" if operation == Operation::Probe && profile.is_none() => {
                profile = Some(args.next()?.into_string().ok()?)
            }
            _ => return None,
        }
    }
    Some(Options {
        timeout: timeout.unwrap_or(Duration::from_secs(10)),
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
    if command.as_deref() == Some(OsStr::new("capacity-inspect")) {
        return capacity::run(args);
    }
    if command.as_deref() == Some(OsStr::new("blob-scrub")) {
        return scrub::run(args, false);
    }
    if command.as_deref() == Some(OsStr::new("blob-scrub-status")) {
        return scrub::run(args, true);
    }
    if command.as_deref() == Some(OsStr::new("blob-scrub-page")) {
        return blob_scrub::run(args);
    }
    if command.as_deref() == Some(OsStr::new("summary-backfill")) {
        return summary_backfill::run(args);
    }
    if command.as_deref() == Some(OsStr::new("search-prepare-page")) {
        return search_prepare::run(args);
    }
    for (name, operation) in [
        ("search-prepare", search_job::Operation::Step),
        ("search-prepare-status", search_job::Operation::Status),
        ("search-prepare-resume", search_job::Operation::Resume),
    ] {
        if command.as_deref() == Some(OsStr::new(name)) {
            return search_job::run(args, operation);
        }
    }
    if let Some(command) = command.as_deref().and_then(account_admin::Command::parse) {
        return account_admin::run(command, args);
    }
    if command.as_deref() == Some(OsStr::new("recovery-release")) {
        return recovery::run(args);
    }
    if command.as_deref() == Some(OsStr::new("logs")) {
        return logs::run(args);
    }
    if let Some(operation) = command.as_deref().and_then(replica::Operation::parse) {
        return replica::run(operation, args);
    }
    if let Some(operation) = command
        .as_deref()
        .and_then(replica_source::Operation::parse)
    {
        return replica_source::run(operation, args);
    }
    if let Some(operation) = command.as_deref().and_then(maintenance::Operation::parse) {
        return maintenance::run(operation, args);
    }
    if command.as_deref() == Some(OsStr::new("metadata-audit")) {
        return metadata_audit::run(args);
    }
    if command.as_deref() == Some(OsStr::new("blob-audit")) {
        return blob_audit::run(args);
    }
    if command.as_deref() == Some(OsStr::new("account-audit")) {
        return audit::run(args);
    }
    if command.as_deref() == Some(OsStr::new("usage-inspect")) {
        return usage::run(args);
    }
    if let Some(operation) = command.as_deref().and_then(selection::Operation::parse) {
        return selection::run(operation, args);
    }
    if let Some(operation) = command.as_deref().and_then(account::Operation::parse) {
        return account::run(operation, args);
    }
    if command.as_deref() == Some(OsStr::new("device-create")) {
        return device_create::run(args);
    }
    if command.as_deref() == Some(OsStr::new("device-revoke")) {
        return device_revoke::run(args);
    }
    if command.as_deref() == Some(OsStr::new("device-list")) {
        return device_list::run(args);
    }
    if command.as_deref() == Some(OsStr::new("config-snapshot")) {
        return configuration_snapshot::run(args);
    }
    if command.as_deref() == Some(OsStr::new("config-converge")) {
        return configuration_converge::run(args);
    }
    if let Some(operation) = command.as_deref().and_then(configuration::Operation::parse) {
        return configuration::run(operation, args);
    }
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
    match check(&loaded, operation, options.timeout) {
        Ok(Outcome::Probe(status)) => {
            writeln!(
                io::stdout().lock(),
                "{{\"schema\":1,\"command\":\"{name}\",\"status\":\"ok\",\"server_version\":{},\"schema_checked\":false,\"profile_checked\":false}}",
                status.server_version
            )?;
            Ok(ExitCode::SUCCESS)
        }
        Ok(Outcome::Schema(receipt)) => {
            let encoded = receipt.store_epoch;
            writeln!(
                io::stdout().lock(),
                "{{\"schema\":1,\"command\":\"{name}\",\"status\":\"ok\",\"schema_version\":{},\"store_epoch\":\"{encoded}\",\"ledger_checked\":true,\"serving_schema_checked\":false,\"profile_checked\":false}}",
                receipt.version
            )?;
            Ok(ExitCode::SUCCESS)
        }
        Ok(Outcome::Plan(receipt)) => {
            let mut out = io::stdout().lock();
            write!(out, "{{\"schema\":1,\"command\":\"{name}\",\"status\":\"ok\",\"current\":{},\"target\":{},\"store_epoch\":\"{}\",\"pending\":[", receipt.version, postgres_schema::VERSION, receipt.store_epoch)?;
            for (index, version) in (receipt.version + 1..=postgres_schema::VERSION).enumerate() {
                if index > 0 {
                    write!(out, ",")?;
                }
                write!(out, "{version}")?;
            }
            write!(out, "],\"schema_usage\":\"td_mta\",\"primary_grants\":{{")?;
            for (index, (table, grants)) in
                td_mta::postgres_runtime_access::primary_grants().enumerate()
            {
                if index > 0 {
                    write!(out, ",")?;
                }
                write!(out, "\"{table}\":\"{grants}\"")?;
            }
            write!(out, "}},\"service_grants\":{{")?;
            for (index, role) in td_mta::postgres_runtime_access::ServiceRole::ALL
                .iter()
                .enumerate()
            {
                if index > 0 {
                    write!(out, ",")?;
                }
                write!(out, "\"{}\":{{", role.name())?;
                for (index, (table, grants)) in role.grants().enumerate() {
                    if index > 0 {
                        write!(out, ",")?;
                    }
                    write!(out, "\"{table}\":\"{grants}\"")?;
                }
                write!(out, "}}")?;
            }
            writeln!(out, "}}}}")?;
            Ok(ExitCode::SUCCESS)
        }
        Ok(Outcome::Runtime(receipt)) => {
            let profile_checked = operation == Operation::ServiceProfileCheck;
            writeln!(
                io::stdout().lock(),
                "{{\"schema\":1,\"command\":\"{name}\",\"status\":\"ok\",\"schema_version\":{},\"store_epoch\":\"{}\",\"runtime_privileges_checked\":true,\"profile_checked\":{profile_checked}}}",
                receipt.version,
                receipt.store_epoch
            )?;
            Ok(ExitCode::SUCCESS)
        }
        Err((stage, code)) => {
            report_error(name, stage, code)?;
            Ok(ExitCode::FAILURE)
        }
    }
}

fn report_error(command: &str, stage: &str, code: &str) -> io::Result<()> {
    writeln!(
        io::stdout().lock(),
        "{{\"schema\":1,\"command\":\"{command}\",\"status\":\"error\",\"stage\":\"{stage}\",\"error\":\"{code}\"}}"
    )
}

enum Outcome {
    Probe(postgres_probe::Status),
    Schema(postgres_schema::Receipt),
    Plan(postgres_schema::Receipt),
    Runtime(postgres_schema::Receipt),
}

fn check(
    loaded: &Loaded,
    operation: Operation,
    timeout: Duration,
) -> Result<Outcome, (&'static str, &'static str)> {
    with_connection_timeout(loaded, timeout, |request, deadline| {
        if matches!(
            operation,
            Operation::RuntimeCheck | Operation::ServiceProfileCheck
        ) {
            let receipt =
                td_mta::postgres_runtime_access::inspect(request, deadline).map_err(|error| {
                    use td_mta::postgres_runtime_access::Error;
                    match error {
                        Error::Connection(error) => {
                            AttemptError::from(postgres_schema::Error::Connection(error))
                        }
                        Error::Schema(error) => AttemptError::from(error),
                        Error::Role => AttemptError::Failed("runtime", "owner-or-privileged-role"),
                        Error::Maintenance => {
                            AttemptError::Failed("runtime", "maintenance-barrier")
                        }
                        Error::Replica => AttemptError::Failed("runtime", "replica-read-only"),
                        Error::Privileges => {
                            AttemptError::Failed("runtime", "incorrect-data-privileges")
                        }
                        Error::Database => {
                            AttemptError::Failed("runtime", "privilege-check-failed")
                        }
                    }
                })?;
            if operation == Operation::ServiceProfileCheck {
                use td_mta::{generations::GenerationSet, postgres_runtime_access::ServiceRole};
                let role = ServiceRole::for_login(request.user)
                    .filter(|role| *role != ServiceRole::Acme)
                    .ok_or(AttemptError::Failed("profile", "serving-login-required"))?;
                let runtime = postgres_probe::Request {
                    mode: postgres_probe::Mode::Runtime,
                    ..*request
                };
                let generations = GenerationSet::at_startup();
                td_mta::postgres_runtime_profile::load(
                    &runtime,
                    deadline,
                    receipt.store_epoch,
                    role.name(),
                    GenerationSet::reserve(&generations)
                        .map_err(|_| AttemptError::Failed("profile", "generation-capacity"))?,
                )
                .map_err(|_| AttemptError::Failed("profile", "profile-preparation-failed"))?;
            }
            return Ok(Outcome::Runtime(receipt));
        }
        match operation {
            Operation::Probe => postgres_probe::probe(request, deadline)
                .map(Outcome::Probe)
                .map_err(postgres_schema::Error::Connection),
            Operation::Initialize => {
                postgres_schema::initialize(request, deadline).map(Outcome::Schema)
            }
            Operation::Upgrade => postgres_schema::upgrade(request, deadline).map(Outcome::Schema),
            Operation::Plan => postgres_schema::plan(request, deadline).map(Outcome::Plan),
            Operation::Inspect => postgres_schema::inspect(request, deadline).map(Outcome::Schema),
            Operation::RuntimeCheck | Operation::ServiceProfileCheck => {
                Err(postgres_schema::Error::Invalid)
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

fn with_connection<T>(
    loaded: &Loaded,
    operation: impl FnOnce(&postgres_probe::Request<'_>, Instant) -> Result<T, AttemptError>,
) -> Result<T, (&'static str, &'static str)> {
    with_connection_timeout(loaded, Duration::from_secs(10), operation)
}

fn with_connection_timeout<T>(
    loaded: &Loaded,
    timeout: Duration,
    operation: impl FnOnce(&postgres_probe::Request<'_>, Instant) -> Result<T, AttemptError>,
) -> Result<T, (&'static str, &'static str)> {
    // Cold files precede one shared connection/operation deadline.
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or(("arguments", "invalid-timeout"))?;
    operation(&loaded.bootstrap.request(), deadline).map_err(|error| match error {
        AttemptError::Transport => refused(deadline),
        AttemptError::Failed(stage, code) => (stage, code),
    })
}

fn classify(error: postgres_schema::Error) -> (&'static str, &'static str) {
    use postgres_schema::Error;
    match error {
        Error::Connection(error) => (
            "database",
            match error {
                postgres_probe::Error::Invalid => "invalid-request",
                postgres_probe::Error::Deadline => "deadline",
                postgres_probe::Error::Database => "connection-or-authentication",
                postgres_probe::Error::Settings => "primary-or-durability-settings",
                postgres_probe::Error::RuntimeSchema => "runtime-schema-qualification",
                postgres_probe::Error::RuntimeRole => "runtime-owner-or-privileged-role",
                postgres_probe::Error::RuntimePrivileges => "runtime-data-privileges",
                postgres_probe::Error::RuntimeMaintenance => "maintenance-barrier",
                postgres_probe::Error::RuntimeReplica => "replica-read-only",
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
                Error::Owner => "owner-required",
                Error::Connection(_) => "connection-or-authentication",
            },
        ),
    }
}

fn refused(deadline: Instant) -> (&'static str, &'static str) {
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
        for operation in [
            "schema-init",
            "schema-inspect",
            "schema-plan",
            "schema-upgrade",
            "runtime-check",
            "service-profile-check",
        ] {
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
    fn only_upgrade_accepts_a_bounded_maintenance_timeout() {
        let args = |op, seconds| {
            parse(&[
                op,
                "--connection-file",
                "/private/db",
                "--timeout-seconds",
                seconds,
            ])
        };
        assert_eq!(
            args("schema-upgrade", "3600").map(|options| options.timeout),
            Some(Duration::from_secs(3600))
        );
        assert!(args("schema-upgrade", "0").is_none());
        assert!(args("schema-upgrade", "3601").is_none());
        assert!(args("schema-init", "60").is_none());
    }
    #[test]
    fn a_transport_refusal_distinguishes_deadline_from_transport() {
        assert_eq!(refused(Instant::now()), ("database", "deadline"));
        assert_eq!(
            refused(Instant::now() + Duration::from_secs(60)),
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
