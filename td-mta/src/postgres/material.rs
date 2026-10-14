//! Explicit immutable material preparation; no configuration activation.
use super::{report_error, with_addresses, AttemptError};
use std::{
    ffi::{OsStr, OsString},
    io::{self, Write},
    process::ExitCode,
};
use td_mta::{
    ids::StoreEpoch,
    postgres_input::MaterialInput,
    postgres_materials::{self, Error, Kind, Version},
    postgres_probe,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Operation {
    Version,
    Import,
    Check,
}
impl Operation {
    pub(super) fn parse(value: &OsStr) -> Option<Self> {
        match value.to_str()? {
            "material-version" => Some(Self::Version),
            "material-import" => Some(Self::Import),
            "material-check" => Some(Self::Check),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Version => "postgres-material-version",
            Self::Import => "postgres-material-import",
            Self::Check => "postgres-material-check",
        }
    }
}
struct Options {
    connection: String,
    input: String,
    epoch: StoreEpoch,
    version: Version,
    kind: Kind,
}
fn kind(value: &str) -> Option<Kind> {
    match value {
        "relay-password" => Some(Kind::RelayPassword),
        "certificate-chain" => Some(Kind::CertificateChain),
        "private-key" => Some(Kind::PrivateKey),
        "trust-bundle" => Some(Kind::TrustBundle),
        "text-signature" => Some(Kind::TextSignature),
        "html-signature" => Some(Kind::HtmlSignature),
        _ => None,
    }
}
fn arguments(mut args: impl Iterator<Item = OsString>) -> Option<Options> {
    let (mut connection, mut input, mut epoch, mut version, mut selected_kind) =
        (None, None, None, None, None);
    while let Some(flag) = args.next() {
        let value = args.next()?.into_string().ok()?;
        match flag.to_str()? {
            "--connection-file" if connection.is_none() => connection = Some(value),
            "--input-file" if input.is_none() => input = Some(value),
            "--epoch" if epoch.is_none() => epoch = Some(StoreEpoch::parse(&value).ok()?),
            "--version" if version.is_none() => version = Some(Version::parse(&value).ok()?),
            "--kind" if selected_kind.is_none() => selected_kind = Some(kind(&value)?),
            _ => return None,
        }
    }
    Some(Options {
        connection: connection?,
        input: input?,
        epoch: epoch?,
        version: version?,
        kind: selected_kind?,
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
        return match postgres_materials::fresh_version() {
            Ok(version) => {
                writeln!(io::stdout().lock(), "{{\"schema\":1,\"command\":\"{name}\",\"status\":\"ok\",\"version\":\"{version}\"}}")?;
                Ok(ExitCode::SUCCESS)
            }
            Err(_) => {
                report_error(name, "material", "crypto")?;
                Ok(ExitCode::FAILURE)
            }
        };
    }
    let Some(options) = arguments(args) else {
        report_error(name, "arguments", "invalid-arguments")?;
        return Ok(ExitCode::from(2));
    };
    let loaded = match MaterialInput::open(&options.connection, &options.input, options.kind) {
        Ok(loaded) => loaded,
        Err(error) => {
            report_error(name, "input", &error.to_string())?;
            return Ok(ExitCode::FAILURE);
        }
    };
    let connection = loaded.connection();
    let result = with_addresses(connection, |request, deadline| {
        match operation {
            Operation::Import => postgres_materials::insert(
                request,
                &connection.trust,
                deadline,
                options.epoch,
                options.version,
                options.kind,
                loaded.bytes(),
            ),
            Operation::Check => postgres_materials::read(
                request,
                &connection.trust,
                deadline,
                options.epoch,
                options.version,
                options.kind,
            )
            .and_then(|material| {
                if material.bytes() == loaded.bytes() {
                    Ok(())
                } else {
                    Err(Error::Integrity)
                }
            }),
            Operation::Version => Err(Error::Invalid),
        }
        .map_err(classify)
    });
    match result {
        Ok(()) => {
            writeln!(io::stdout().lock(), "{{\"schema\":1,\"command\":\"{name}\",\"status\":\"ok\",\"store_epoch\":\"{}\",\"version\":\"{}\",\"content_validated\":false,\"profile_activated\":false}}", options.epoch, options.version)?;
            Ok(ExitCode::SUCCESS)
        }
        Err((stage, code)) => {
            report_error(name, stage, code)?;
            Ok(ExitCode::FAILURE)
        }
    }
}
pub(super) fn classify(error: Error) -> AttemptError {
    match error {
        Error::Schema(error) => error.into(),
        Error::Connection(postgres_probe::Error::Transport) => AttemptError::Transport,
        Error::Connection(error) => td_mta::postgres_schema::Error::Connection(error).into(),
        other => AttemptError::Failed(
            "material",
            match other {
                Error::Epoch => "store-epoch-mismatch",
                Error::Invalid => "invalid-material",
                Error::Missing => "material-missing",
                Error::Kind => "material-kind-mismatch",
                Error::Integrity => "material-integrity-or-content-mismatch",
                Error::Capacity => "capacity",
                Error::Crypto => "crypto",
                Error::Database => "material-operation-failed",
                Error::Indeterminate => "indeterminate-do-not-retry",
                Error::Schema(_) | Error::Connection(_) => "connection-or-schema",
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arguments_require_exact_kind_epoch_version_and_protected_paths() {
        let valid = [
            "--connection-file",
            "/private/connection",
            "--input-file",
            "/private/input",
            "--epoch",
            "01010101010101010101010101010101",
            "--version",
            "02020202020202020202020202020202",
            "--kind",
            "private-key",
        ];
        assert!(arguments(valid.iter().map(OsString::from)).is_some());
        for end in 0..valid.len() {
            assert!(arguments(valid.iter().take(end).map(OsString::from)).is_none());
        }
        for suffix in [
            vec!["--kind", "private-key"],
            vec!["--profile", "desktop"],
            vec!["--password", "secret"],
        ] {
            assert!(arguments(valid.iter().copied().chain(suffix).map(OsString::from)).is_none());
        }
        for bad in ["", "AA010101010101010101010101010101", "../private", "0"] {
            assert!(StoreEpoch::parse(bad).is_err());
            assert!(Version::parse(bad).is_err());
        }
        assert!(matches!(
            classify(Error::Indeterminate),
            AttemptError::Failed("material", "indeterminate-do-not-retry")
        ));
        assert!(matches!(
            classify(Error::Connection(postgres_probe::Error::Transport)),
            AttemptError::Transport
        ));
    }
}
