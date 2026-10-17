//! Explicit owner-side credential creation in the preparation database.
use super::{report_error, with_addresses, AttemptError};
use std::{
    ffi::OsString,
    io::{self, Write},
    process::ExitCode,
    time::{SystemTime, UNIX_EPOCH},
};
use td_mta::{
    ids::{AccountId, StoreEpoch},
    postgres_devices::{self, Error},
    postgres_input::Loaded,
    postgres_schema,
};

const NAME: &str = "postgres-device-create";
struct Options {
    connection: String,
    epoch: StoreEpoch,
    account: AccountId,
    label: String,
}
fn arguments(mut args: impl Iterator<Item = OsString>) -> Option<Options> {
    let (mut connection, mut epoch, mut account, mut label) = (None, None, None, None);
    while let Some(flag) = args.next() {
        let value = args.next()?.into_string().ok()?;
        match flag.to_str()? {
            "--connection-file" if connection.is_none() => connection = Some(value),
            "--epoch" if epoch.is_none() => epoch = Some(StoreEpoch::parse(&value).ok()?),
            "--account" if account.is_none() => account = Some(AccountId::parse(&value).ok()?),
            "--label" if label.is_none() => label = Some(value),
            _ => return None,
        }
    }
    Some(Options {
        connection: connection?,
        epoch: epoch?,
        account: account?,
        label: label?,
    })
}
pub(super) fn run(args: impl Iterator<Item = OsString>) -> io::Result<ExitCode> {
    let Some(options) = arguments(args) else {
        report_error(NAME, "arguments", "invalid-arguments")?;
        return Ok(ExitCode::from(2));
    };
    let loaded = match Loaded::open(&options.connection, "devices") {
        Ok(loaded) => loaded,
        Err(error) => {
            report_error(NAME, "bootstrap", &error.to_string())?;
            return Ok(ExitCode::FAILURE);
        }
    };
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|value| u64::try_from(value.as_millis()).ok());
    let Some(timestamp) = timestamp else {
        report_error(NAME, "devices", "invalid-clock")?;
        return Ok(ExitCode::FAILURE);
    };
    let operation = match postgres_devices::prepare_creation(
        options.epoch,
        options.account,
        &options.label,
        timestamp,
    ) {
        Ok(operation) => operation,
        Err(error) => {
            let AttemptError::Failed(stage, code) = classify(error) else {
                report_error(NAME, "devices", "preparation-failed")?;
                return Ok(ExitCode::FAILURE);
            };
            report_error(NAME, stage, code)?;
            return Ok(ExitCode::FAILURE);
        }
    };
    match with_addresses(&loaded, |request, deadline| {
        postgres_devices::publish_creation(request, &loaded.trust, deadline, &operation)
            .map_err(classify)
    }) {
        Ok(()) => {
            let mut out = io::stdout().lock();
            write!(out, "{{\"schema\":1,\"command\":\"{NAME}\",\"status\":\"ok\",\"store_epoch\":\"{}\",\"account\":\"{}\",\"device\":\"{}\",\"password\":\"", options.epoch, options.account, operation.device_id())?;
            out.write_all(operation.password())?;
            writeln!(out, "\"}}")?;
            Ok(ExitCode::SUCCESS)
        }
        Err((stage, code)) => {
            writeln!(io::stdout().lock(), "{{\"schema\":1,\"command\":\"{NAME}\",\"status\":\"error\",\"stage\":\"{stage}\",\"error\":\"{code}\",\"store_epoch\":\"{}\",\"account\":\"{}\",\"device\":\"{}\"}}", options.epoch, options.account, operation.device_id())?;
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
        Error::Epoch => "epoch-mismatch",
        Error::Missing => "account-not-found",
        Error::Invalid => "invalid-device-operation",
        Error::Capacity => "device-capacity",
        Error::Database => "query-failed",
        Error::Conflict => "conflict",
        Error::Crypto => "crypto",
        Error::Indeterminate => "indeterminate-inspect-and-revoke-device",
    };
    AttemptError::Failed("devices", code)
}
