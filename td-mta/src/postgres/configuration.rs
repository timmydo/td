//! Explicit source-version preparation; never activates a configuration.
use super::{report_error, with_addresses, AttemptError};
use std::{
    ffi::{OsStr, OsString},
    io::{self, Write},
    process::ExitCode,
};
use td_mta::{
    config::{profile::Roles, values},
    ids::StoreEpoch,
    postgres_configuration::{self as configuration, Candidate},
    postgres_configuration_store::{self as versions, Error, Version},
    postgres_input::{ConfigurationInput, Loaded, SourceFile},
    postgres_probe,
};

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Operation {
    Version,
    Import,
    Check,
}
impl Operation {
    pub(super) fn parse(value: &OsStr) -> Option<Self> {
        match value.to_str()? {
            "config-version" => Some(Self::Version),
            "config-import" => Some(Self::Import),
            "config-check" => Some(Self::Check),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Version => "postgres-config-version",
            Self::Import => "postgres-config-import",
            Self::Check => "postgres-config-check",
        }
    }
}
struct Instance {
    name: String,
    roles: Roles,
    path: String,
}
struct Options {
    connection: String,
    epoch: StoreEpoch,
    version: Version,
    shared: Option<String>,
    instances: Vec<Instance>,
}
fn roles(text: &str) -> Option<Roles> {
    let mut bits = 0;
    for role in text.split(',') {
        let bit = match role {
            "receive" => Roles::RECEIVE,
            "jmap" => Roles::JMAP,
            "relay" => Roles::RELAY,
            _ => return None,
        };
        if bits & bit != 0 {
            return None;
        }
        bits |= bit;
    }
    Roles::from_bits(bits)
}
fn arguments(operation: Operation, mut args: impl Iterator<Item = OsString>) -> Option<Options> {
    let (mut connection, mut epoch, mut version, mut shared) = (None, None, None, None);
    let mut instances: Vec<Instance> = Vec::new();
    while let Some(flag) = args.next() {
        let value = args.next()?.into_string().ok()?;
        match flag.to_str()? {
            "--connection-file" if connection.is_none() => connection = Some(value),
            "--epoch" if epoch.is_none() => epoch = Some(StoreEpoch::parse(&value).ok()?),
            "--version" if version.is_none() => version = Some(Version::parse(&value).ok()?),
            "--shared-file" if operation == Operation::Import && shared.is_none() => {
                shared = Some(value)
            }
            "--instance"
                if operation == Operation::Import
                    && instances.len() < configuration::MAX_PROFILES =>
            {
                values::profile_name(&value).ok()?;
                if instances.iter().any(|instance| instance.name == value) {
                    return None;
                }
                let roles = roles(&args.next()?.into_string().ok()?)?;
                let path = args.next()?.into_string().ok()?;
                instances.try_reserve(1).ok()?;
                instances.push(Instance {
                    name: value,
                    roles,
                    path,
                });
            }
            _ => return None,
        }
    }
    if operation == Operation::Import && (shared.is_none() || instances.is_empty()) {
        return None;
    }
    Some(Options {
        connection: connection?,
        epoch: epoch?,
        version: version?,
        shared,
        instances,
    })
}

pub(super) fn run(
    operation: Operation,
    mut args: impl Iterator<Item = OsString>,
) -> io::Result<ExitCode> {
    let name = operation.name();
    if operation == Operation::Version {
        if args.next().is_some() {
            report_error(name, "arguments", "invalid-arguments")?;
            return Ok(ExitCode::from(2));
        }
        return match versions::fresh_version() {
            Ok(version) => {
                writeln!(io::stdout().lock(), "{{\"schema\":1,\"command\":\"{name}\",\"status\":\"ok\",\"version\":\"{version}\"}}")?;
                Ok(ExitCode::SUCCESS)
            }
            Err(_) => {
                report_error(name, "configuration", "crypto")?;
                Ok(ExitCode::FAILURE)
            }
        };
    }
    let Some(options) = arguments(operation, args) else {
        report_error(name, "arguments", "invalid-arguments")?;
        return Ok(ExitCode::from(2));
    };
    let result = match operation {
        Operation::Import => import(&options),
        Operation::Check => check(&options),
        Operation::Version => Err(("arguments", "invalid-arguments")),
    };
    match result {
        Ok(candidate) => {
            let mut encoded = String::new();
            for byte in candidate.digest() {
                use std::fmt::Write;
                write!(&mut encoded, "{byte:02x}")
                    .map_err(|_| io::Error::other("configuration digest"))?;
            }
            writeln!(io::stdout().lock(), "{{\"schema\":1,\"command\":\"{name}\",\"status\":\"ok\",\"store_epoch\":\"{}\",\"version\":\"{}\",\"digest\":\"{encoded}\",\"profiles\":{},\"content_validated\":true,\"profile_activated\":false}}", options.epoch, options.version, candidate.profile_count())?;
            Ok(ExitCode::SUCCESS)
        }
        Err((stage, code)) => {
            report_error(name, stage, code)?;
            Ok(ExitCode::FAILURE)
        }
    }
}
fn input_error(error: td_mta::postgres_input::Error) -> (&'static str, &'static str) {
    ("input", error.code())
}

fn import(options: &Options) -> Result<Candidate, (&'static str, &'static str)> {
    let shared = options
        .shared
        .as_deref()
        .ok_or(("arguments", "invalid-arguments"))?;
    let mut sources = Vec::new();
    sources
        .try_reserve_exact(options.instances.len())
        .map_err(|_| ("input", "capacity"))?;
    for instance in &options.instances {
        sources.push(SourceFile {
            name: &instance.name,
            roles: instance.roles,
            path: &instance.path,
        });
    }
    let loaded =
        ConfigurationInput::open(&options.connection, shared, &sources).map_err(input_error)?;
    let connection = loaded.connection();
    let mut profiles = Vec::new();
    profiles
        .try_reserve_exact(loaded.profile_count())
        .map_err(|_| ("input", "capacity"))?;
    for index in 0..loaded.profile_count() {
        profiles.push(loaded.profile(index).ok_or(("input", "inventory"))?);
    }
    with_addresses(connection, |request, deadline| {
        let candidate = configuration::prepare(
            request,
            &connection.trust,
            deadline,
            options.epoch,
            loaded.shared(),
            &profiles,
        )
        .map_err(|error| classify(Error::Validation(error)))?;
        versions::publish(
            request,
            &connection.trust,
            deadline,
            options.version,
            &candidate,
        )
        .map_err(classify)?;
        Ok(candidate)
    })
}
fn check(options: &Options) -> Result<Candidate, (&'static str, &'static str)> {
    let loaded = Loaded::open(&options.connection, "configuration").map_err(input_error)?;
    with_addresses(&loaded, |request, deadline| {
        versions::read(
            request,
            &loaded.trust,
            deadline,
            options.epoch,
            options.version,
        )
        .map_err(classify)
    })
}
pub(super) fn classify(error: Error) -> AttemptError {
    match error {
        Error::Schema(error) => error.into(),
        Error::Connection(postgres_probe::Error::Transport) => AttemptError::Transport,
        Error::Connection(error) => td_mta::postgres_schema::Error::Connection(error).into(),
        Error::Validation(configuration::Error::Material(
            td_mta::postgres_profile_materials::Error::Database(error),
        )) => super::material::classify(error),
        other => AttemptError::Failed(
            "configuration",
            match other {
                Error::Validation(configuration::Error::Deadline)
                | Error::Validation(configuration::Error::Material(
                    td_mta::postgres_profile_materials::Error::Deadline,
                )) => "deadline",
                Error::Validation(configuration::Error::Provider) => "provider-validation",
                Error::Validation(_) => "source-or-material-validation",
                Error::Epoch => "store-epoch-mismatch",
                Error::Missing => "configuration-missing",
                Error::Conflict => "version-content-conflict",
                Error::Integrity => "configuration-integrity",
                Error::Capacity => "capacity",
                Error::Crypto => "crypto",
                Error::Database => "configuration-operation-failed",
                Error::Indeterminate => "indeterminate-retain-version",
                Error::Connection(_) | Error::Schema(_) => "connection-or-schema",
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn material_loader_deadline_keeps_the_deadline_diagnostic() {
        for error in [
            configuration::Error::Deadline,
            configuration::Error::Material(td_mta::postgres_profile_materials::Error::Deadline),
        ] {
            assert!(matches!(
                classify(Error::Validation(error)),
                AttemptError::Failed("configuration", "deadline")
            ));
        }
    }

    #[test]
    fn role_names_are_explicit_nonempty_and_unique() {
        for (text, bits) in [
            ("receive", 1),
            ("jmap", 2),
            ("relay", 4),
            ("receive,jmap,relay", 7),
            ("relay,receive", 5),
        ] {
            assert_eq!(roles(text).map(|roles| roles.bits()), Some(bits));
        }
        for text in [
            "",
            "7",
            "smtp",
            "JMAP",
            "jmap,jmap",
            "jmap,",
            ",relay",
            "receive, jmap",
        ] {
            assert!(roles(text).is_none());
        }
    }
    #[test]
    fn import_and_check_require_complete_distinct_argument_shapes() {
        let common = [
            "--connection-file",
            "/private/db",
            "--epoch",
            "01010101010101010101010101010101",
            "--version",
            "02020202020202020202020202020202",
        ];
        let input = [
            "--shared-file",
            "/private/shared",
            "--instance",
            "desktop",
            "jmap",
            "/private/profile",
        ];
        assert!(arguments(Operation::Check, common.iter().map(OsString::from)).is_some());
        assert!(arguments(Operation::Import, common.iter().map(OsString::from)).is_none());
        assert!(arguments(
            Operation::Import,
            common.iter().chain(input.iter()).map(OsString::from)
        )
        .is_some());
        assert!(arguments(
            Operation::Check,
            common.iter().chain(input.iter()).map(OsString::from)
        )
        .is_none());
        for suffix in [
            vec!["--instance", "desktop", "jmap", "/other"],
            vec!["--profile", "desktop"],
            vec!["--shared-file", "/other"],
            vec!["--connection", "secret"],
        ] {
            assert!(arguments(
                Operation::Import,
                common
                    .iter()
                    .chain(input.iter())
                    .chain(suffix.iter())
                    .map(OsString::from)
            )
            .is_none());
        }
        for end in 0..input.len() {
            assert!(arguments(
                Operation::Import,
                common
                    .iter()
                    .chain(input.iter().take(end))
                    .map(OsString::from)
            )
            .is_none());
        }
    }
}
