//! Offline device administration under both store locks.
use super::*;
use td_mta::{
    ids::DeviceId,
    store_fs::{DeviceError, DeviceStore},
};

struct Options {
    root: String,
    account: AccountId,
    operation: Operation,
}
enum Operation {
    Create(String),
    Revoke(DeviceId),
    List,
}
fn arguments(mut args: impl Iterator<Item = std::ffi::OsString>) -> Option<Options> {
    let command = args.next()?.into_string().ok()?;
    let mut root = None;
    let mut account = None;
    let mut label = None;
    let mut id = None;
    while let Some(flag) = args.next() {
        let value = args.next()?.into_string().ok()?;
        match flag.to_str()? {
            "--root" if root.is_none() => root = Some(value),
            "--account" if account.is_none() => account = Some(AccountId::parse(&value).ok()?),
            "--label" if label.is_none() => label = Some(value),
            "--id" if id.is_none() => id = Some(DeviceId::parse(&value).ok()?),
            _ => return None,
        }
    }
    let operation = match command.as_str() {
        "create" if id.is_none() => Operation::Create(label?),
        "revoke" if label.is_none() => Operation::Revoke(id?),
        "list" if label.is_none() && id.is_none() => Operation::List,
        _ => return None,
    };
    Some(Options {
        root: root?,
        account: account?,
        operation,
    })
}
fn failed(error: DeviceError) -> Failure {
    Failure {
        stage: "devices",
        code: match error {
            DeviceError::Invalid => "invalid",
            DeviceError::Capacity => "capacity",
            DeviceError::NotFound => "not-found",
            DeviceError::Unavailable => "unavailable",
        },
    }
}
fn execute(options: Options, output: &mut impl Write) -> Result<(), Failure> {
    let clock = Arc::new(RuntimeClock::new());
    let deadline = scope(clock.as_ref(), 600_000)?;
    let mut root = locked_root(&options.root, "store-root", "store-lock")?;
    let store = IndexStore::open(&mut root, clock.clone(), 1, deadline)
        .map_err(|error| adapter("store-open", error))?;
    let accounts = store
        .account_ids(deadline)
        .map_err(|error| adapter("accounts", error))?;
    if accounts.as_slice() != [options.account] {
        return Err(Failure {
            stage: "account",
            code: "not-found",
        });
    }
    let mut devices_root = locked_root(
        &format!("{}/devices", options.root),
        "device-root",
        "device-lock",
    )?;
    let now = u64::try_from(
        clock
            .sample()
            .map_err(|error| adapter("clock", error))?
            .utc_ms,
    )
    .map_err(|_| Failure {
        stage: "clock",
        code: "invalid",
    })?;
    let io_failure = |_| Failure {
        stage: "receipt",
        code: "io",
    };
    match options.operation {
        Operation::Create(label) => {
            let mut entropy = td_crypto::SystemEntropy::try_new()
                .map_err(|_| failed(DeviceError::Unavailable))?;
            let mut devices =
                DeviceStore::open_or_create(&mut devices_root, options.account).map_err(failed)?;
            let (id, password) = devices.create(&label, now, &mut entropy).map_err(failed)?;
            write!(
                output,
                "{{\"schema\":1,\"command\":\"device.create\",\"status\":\"ok\",\"deviceId\":\"{id}\",\"password\":\""
            )
            .map_err(io_failure)?;
            output.write_all(password.expose()).map_err(io_failure)?;
            writeln!(output, "\"}}").map_err(io_failure)?;
        }
        Operation::Revoke(id) => {
            let mut devices =
                DeviceStore::open(&mut devices_root, options.account).map_err(failed)?;
            devices.revoke(id, now).map_err(failed)?;
            writeln!(
                output,
                "{{\"schema\":1,\"command\":\"device.revoke\",\"status\":\"ok\",\"deviceId\":\"{id}\",\"revoked\":true}}"
            )
            .map_err(io_failure)?;
        }
        Operation::List => {
            let devices = DeviceStore::open(&mut devices_root, options.account).map_err(failed)?;
            write!(
                output,
                "{{\"schema\":1,\"command\":\"device.list\",\"status\":\"ok\",\"deviceIds\":["
            )
            .map_err(io_failure)?;
            for (index, id) in devices.device_ids().map_err(failed)?.enumerate() {
                if index != 0 {
                    write!(output, ",").map_err(io_failure)?;
                }
                write!(output, "\"{id}\"").map_err(io_failure)?;
            }
            writeln!(output, "]}}").map_err(io_failure)?;
        }
    }
    output.flush().map_err(io_failure)?;
    Ok(())
}
pub(super) fn run(args: impl Iterator<Item = std::ffi::OsString>) -> io::Result<ExitCode> {
    let mut output = io::stdout().lock();
    let parsed = arguments(args);
    let command = match parsed.as_ref().map(|options| &options.operation) {
        Some(Operation::Create(_)) => "device.create",
        Some(Operation::Revoke(_)) => "device.revoke",
        Some(Operation::List) => "device.list",
        None => "device",
    };
    let result = parsed
        .ok_or(Failure {
            stage: "arguments",
            code: "invalid-arguments",
        })
        .and_then(|options| execute(options, &mut output));
    match result {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(error) => {
            let code = error.exit_code();
            failure(&mut output, command, error)?;
            Ok(code)
        }
    }
}
