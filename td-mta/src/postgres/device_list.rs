//! Explicit read-only inspection of the preparation database's device registry.
use super::{report_error, with_addresses, AttemptError};
use std::{
    ffi::OsString,
    io::{self, Write},
    process::ExitCode,
};
use td_mta::{
    ids::{AccountId, StoreEpoch},
    postgres_devices::{self, DeviceSummary, Error},
    postgres_input::Loaded,
    postgres_schema,
};

const NAME: &str = "postgres-device-list";
struct Options {
    connection: String,
    epoch: StoreEpoch,
    account: AccountId,
}
fn arguments(mut args: impl Iterator<Item = OsString>) -> Option<Options> {
    let (mut connection, mut epoch, mut account) = (None, None, None);
    while let Some(flag) = args.next() {
        let value = args.next()?.into_string().ok()?;
        match flag.to_str()? {
            "--connection-file" if connection.is_none() => connection = Some(value),
            "--epoch" if epoch.is_none() => epoch = Some(StoreEpoch::parse(&value).ok()?),
            "--account" if account.is_none() => account = Some(AccountId::parse(&value).ok()?),
            _ => return None,
        }
    }
    Some(Options {
        connection: connection?,
        epoch: epoch?,
        account: account?,
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
    match with_addresses(&loaded, |request, deadline| {
        postgres_devices::list(
            request,
            &loaded.trust,
            deadline,
            options.epoch,
            options.account,
        )
        .map_err(classify)
    }) {
        Ok(devices) => {
            output(
                &mut io::stdout().lock(),
                options.epoch,
                options.account,
                &devices,
            )?;
            Ok(ExitCode::SUCCESS)
        }
        Err((stage, code)) => {
            report_error(NAME, stage, code)?;
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
        Error::Invalid => "invalid-device-metadata",
        Error::Capacity => "device-capacity",
        Error::Database => "query-failed",
        Error::Conflict => "conflict",
        Error::Crypto => "crypto",
        Error::Indeterminate => "indeterminate",
    };
    AttemptError::Failed("devices", code)
}
fn output(
    out: &mut impl Write,
    epoch: StoreEpoch,
    account: AccountId,
    devices: &[DeviceSummary],
) -> io::Result<()> {
    write!(out, "{{\"schema\":1,\"command\":\"{NAME}\",\"status\":\"ok\",\"store_epoch\":\"{epoch}\",\"account\":\"{account}\",\"devices\":[")?;
    for (index, device) in devices.iter().enumerate() {
        if index != 0 {
            write!(out, ",")?;
        }
        write!(
            out,
            "{{\"id\":\"{}\",\"label\":\"{}\",\"created_at\":{},\"revoked_at\":",
            device.id,
            super::super::json_text(&device.label),
            device.created_at
        )?;
        match device.revoked_at {
            Some(value) => write!(out, "{value}")?,
            None => write!(out, "null")?,
        }
        write!(out, "}}")?;
    }
    writeln!(out, "]}}")
}
