//! Explicit owner-side account preparation and retained receipt inspection.
use super::{report_error, with_addresses, AttemptError};
use std::{
    ffi::{OsStr, OsString},
    io::{self, Write},
    process::ExitCode,
};
use td_mta::{
    ids::{AccountId, StoreEpoch},
    postgres_accounts::{self, Creation, Error},
    postgres_input::Loaded,
    postgres_schema,
};

#[derive(Clone, Copy)]
pub(super) enum Operation {
    Initialize,
    Inspect,
}
impl Operation {
    pub(super) fn parse(value: &OsStr) -> Option<Self> {
        match value.to_str()? {
            "account-init" => Some(Self::Initialize),
            "account-inspect" => Some(Self::Inspect),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Initialize => "postgres-account-init",
            Self::Inspect => "postgres-account-inspect",
        }
    }
}
pub(super) struct Options {
    pub(super) connection: String,
    pub(super) epoch: StoreEpoch,
    pub(super) account: AccountId,
}
pub(super) fn arguments(mut args: impl Iterator<Item = OsString>) -> Option<Options> {
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
pub(super) fn run(
    operation: Operation,
    args: impl Iterator<Item = OsString>,
) -> io::Result<ExitCode> {
    let name = operation.name();
    let Some(options) = arguments(args) else {
        report_error(name, "arguments", "invalid-arguments")?;
        return Ok(ExitCode::from(2));
    };
    let loaded = match Loaded::open(&options.connection, "accounts") {
        Ok(loaded) => loaded,
        Err(error) => {
            report_error(name, "bootstrap", &error.to_string())?;
            return Ok(ExitCode::FAILURE);
        }
    };
    let creation = match operation {
        Operation::Initialize => match postgres_accounts::prepare(options.epoch, options.account) {
            Ok(creation) => Some(creation),
            Err(_) => {
                report_error(name, "accounts", "preparation-failed")?;
                return Ok(ExitCode::FAILURE);
            }
        },
        Operation::Inspect => None,
    };
    let result = with_addresses(&loaded, |request, deadline| {
        match creation {
            Some(creation) => {
                postgres_accounts::publish(request, &loaded.trust, deadline, creation)
                    .map(|()| creation)
            }
            None => postgres_accounts::inspect(
                request,
                &loaded.trust,
                deadline,
                options.epoch,
                options.account,
            ),
        }
        .map_err(classify)
    });
    match result {
        Ok(receipt) => {
            success(name, receipt)?;
            Ok(ExitCode::SUCCESS)
        }
        Err((stage, code)) => {
            if let Some(creation) = creation {
                writeln!(io::stdout().lock(), "{{\"schema\":1,\"command\":\"{name}\",\"status\":\"error\",\"stage\":\"{stage}\",\"error\":\"{code}\",\"store_epoch\":\"{}\",\"account\":\"{}\",\"inbox\":\"{}\"}}", creation.epoch(), creation.account(), creation.inbox())?;
            } else {
                report_error(name, stage, code)?;
            }
            Ok(ExitCode::FAILURE)
        }
    }
}
fn success(name: &str, receipt: Creation) -> io::Result<()> {
    writeln!(io::stdout().lock(), "{{\"schema\":1,\"command\":\"{name}\",\"status\":\"ok\",\"store_epoch\":\"{}\",\"account\":\"{}\",\"inbox\":\"{}\"}}", receipt.epoch(), receipt.account(), receipt.inbox())
}
fn classify(error: Error) -> AttemptError {
    let code = match error {
        Error::Schema(error) => return AttemptError::from(error),
        Error::Connection(error) => {
            return AttemptError::from(postgres_schema::Error::Connection(error))
        }
        Error::Epoch => "epoch-mismatch",
        Error::Missing => "bootstrap-receipt-not-found",
        Error::Exists => "unreceipted-account-exists",
        Error::Conflict => "bootstrap-conflict",
        Error::Integrity => "bootstrap-integrity",
        Error::Crypto => "crypto",
        Error::Database => "query-failed",
        Error::Indeterminate => "indeterminate-inspect-account",
    };
    AttemptError::Failed("accounts", code)
}
