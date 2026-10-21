//! Operator account workflow; immutable retained requests replace ad hoc SQL.
use super::{report_error, with_addresses, AttemptError};
use std::{
    ffi::{OsStr, OsString},
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
    process::ExitCode,
};
use td_crypto::{Entropy, SystemEntropy};
use td_mta::{
    account_configuration::Edit,
    ids::{AccountId, IdentityId, StoreEpoch},
    operator_files::{Inputs, Role},
    postgres_account_admin::{self as admin, Operation},
    postgres_input::Loaded,
    postgres_schema, postgres_selection,
    store_fs::PrivateRoot,
};

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Command {
    Create,
    Update,
    AliasAdd,
    AliasRemove,
    Apply,
    Inspect,
    List,
}
impl Command {
    pub(super) fn parse(value: &OsStr) -> Option<Self> {
        Some(match value.to_str()? {
            "account-create" => Self::Create,
            "account-update" => Self::Update,
            "account-alias-add" => Self::AliasAdd,
            "account-alias-remove" => Self::AliasRemove,
            "account-apply" => Self::Apply,
            "account-operation-inspect" => Self::Inspect,
            "account-list" => Self::List,
            _ => return None,
        })
    }
    fn name(self) -> &'static str {
        match self {
            Self::Create => "postgres-account-create",
            Self::Update => "postgres-account-update",
            Self::AliasAdd => "postgres-account-alias-add",
            Self::AliasRemove => "postgres-account-alias-remove",
            Self::Apply => "postgres-account-apply",
            Self::Inspect => "postgres-account-operation-inspect",
            Self::List => "postgres-account-list",
        }
    }
}
struct Options {
    connection: String,
    operation: Option<String>,
    epoch: Option<StoreEpoch>,
    account: Option<AccountId>,
    address: Option<String>,
    name: Option<String>,
    username: Option<String>,
}
fn arguments(command: Command, mut args: impl Iterator<Item = OsString>) -> Option<Options> {
    let mut o = Options {
        connection: String::new(),
        operation: None,
        epoch: None,
        account: None,
        address: None,
        name: None,
        username: None,
    };
    while let Some(flag) = args.next() {
        let value = args.next()?.into_string().ok()?;
        match flag.to_str()? {
            "--connection-file" if o.connection.is_empty() && !value.is_empty() => {
                o.connection = value
            }
            "--operation-file" if command != Command::List && o.operation.is_none() => {
                o.operation = Some(value)
            }
            "--epoch"
                if !matches!(command, Command::Apply | Command::Inspect) && o.epoch.is_none() =>
            {
                o.epoch = Some(StoreEpoch::parse(&value).ok()?)
            }
            "--account"
                if matches!(
                    command,
                    Command::Update | Command::AliasAdd | Command::AliasRemove
                ) && o.account.is_none() =>
            {
                o.account = Some(AccountId::parse(&value).ok()?)
            }
            "--address"
                if matches!(
                    command,
                    Command::Create | Command::AliasAdd | Command::AliasRemove
                ) && o.address.is_none() =>
            {
                o.address = Some(value)
            }
            "--name"
                if matches!(command, Command::Create | Command::Update) && o.name.is_none() =>
            {
                o.name = Some(value)
            }
            "--username"
                if matches!(command, Command::Create | Command::Update) && o.username.is_none() =>
            {
                o.username = Some(value)
            }
            _ => return None,
        }
    }
    if o.connection.is_empty()
        || (command != Command::List && o.operation.is_none())
        || (matches!(
            command,
            Command::Update | Command::AliasAdd | Command::AliasRemove
        ) && o.account.is_none())
        || (matches!(
            command,
            Command::Create | Command::AliasAdd | Command::AliasRemove
        ) && o.address.is_none())
        || (command == Command::Update && o.name.is_none() && o.username.is_none())
    {
        return None;
    }
    Some(o)
}
fn file_root(path: &str) -> io::Result<PrivateRoot> {
    let parent = Path::new(path)
        .parent()
        .and_then(Path::to_str)
        .ok_or(io::ErrorKind::InvalidInput)?;
    if Path::new(path).file_name().is_none() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    PrivateRoot::open(parent)
        .map_err(|_| io::Error::other("operation parent must be a private trusted directory"))
}
fn save(path: &str, operation: &Operation) -> io::Result<()> {
    let _root = file_root(path)?;
    let bytes = operation
        .encode()
        .map_err(|_| io::Error::other("operation encoding failed"))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    // Restore owner access removed by umask without granting shared access.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    let parent = Path::new(path)
        .parent()
        .ok_or(io::ErrorKind::InvalidInput)?;
    File::open(parent)?.sync_all()
}
fn load(path: &str, durable: bool) -> io::Result<Operation> {
    let root = file_root(path)?;
    let mut inputs = Inputs::new();
    inputs
        .bind(&root)
        .map_err(|_| io::Error::other("operation root refused"))?;
    let mut file = inputs
        .open(path, Role::Secret)
        .map_err(|_| io::Error::other("operation file refused"))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(admin::MAX_OPERATION_BYTES + 1)
        .map_err(|_| io::Error::other("operation capacity"))?;
    (&mut file)
        .take((admin::MAX_OPERATION_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    let operation =
        Operation::decode(&bytes).map_err(|_| io::Error::other("invalid operation record"))?;
    if durable {
        // A prior attempt may have written a complete record but failed its sync.
        file.sync_all()?;
        File::open(
            Path::new(path)
                .parent()
                .ok_or(io::ErrorKind::InvalidInput)?,
        )?
        .sync_all()?;
    }
    Ok(operation)
}
fn random() -> Result<[u8; 16], (&'static str, &'static str)> {
    let mut bytes = [0; 16];
    SystemEntropy::try_new()
        .and_then(|mut rng| rng.fill(&mut bytes))
        .map_err(|_| ("account-admin", "entropy"))?;
    Ok(bytes)
}
fn classify(error: admin::Error) -> AttemptError {
    use admin::Error;
    match error {
        Error::Selection(postgres_selection::Error::Connection(e)) => {
            postgres_schema::Error::Connection(e).into()
        }
        Error::Selection(postgres_selection::Error::Schema(e)) => e.into(),
        Error::Selection(postgres_selection::Error::Conflict) => {
            AttemptError::Failed("account-admin", "configuration-conflict-retain-operation")
        }
        Error::Selection(postgres_selection::Error::Indeterminate)
        | Error::Account(td_mta::postgres_accounts::Error::Indeterminate)
        | Error::Version(td_mta::postgres_configuration_store::Error::Indeterminate) => {
            AttemptError::Failed("account-admin", "indeterminate-retain-operation")
        }
        _ => AttemptError::Failed(
            "account-admin",
            match error {
                Error::Edit(td_mta::account_configuration::Error::Missing) => {
                    "account-or-alias-not-found"
                }
                Error::Edit(td_mta::account_configuration::Error::Conflict) => {
                    "account-or-alias-conflict"
                }
                Error::MissingSelection => "no-selected-configuration",
                Error::Capacity => "capacity",
                Error::Integrity => "operation-integrity",
                Error::Configuration(_) => "configuration-validation-failed",
                Error::Version(_) => "configuration-storage-failed",
                Error::Selection(_) => "configuration-selection-failed",
                Error::Account(_) => "account-bootstrap-failed",
                Error::Edit(_) => "invalid-account-edit",
            },
        ),
    }
}
fn execute(command: Command, o: &Options) -> Result<(), (&'static str, &'static str)> {
    let loaded = Loaded::open(&o.connection, "account-admin")
        .map_err(|_| ("bootstrap", "connection-input"))?;
    if command == Command::List {
        return list(&loaded, o.epoch);
    }
    let path = o
        .operation
        .as_deref()
        .ok_or(("arguments", "operation-file-required"))?;
    let operation = if matches!(command, Command::Apply | Command::Inspect) {
        load(path, command == Command::Apply)
            .map_err(|_| ("operation-file", "read-sync-or-integrity-failed"))?
    } else {
        // Reject existing operation paths before any database work. Creation still
        // uses create_new to refuse a concurrent creator.
        match std::fs::symlink_metadata(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            _ => {
                return Err((
                    "operation-file",
                    "already-exists-or-unreadable-use-account-apply",
                ))
            }
        }
        file_root(path).map_err(|_| ("operation-file", "private-parent-required"))?;
        let epoch = epoch(&loaded, o.epoch)?;
        let account = match o.account {
            Some(id) => id,
            None => AccountId::from_bytes(random()?),
        };
        let address = o.address.as_deref().unwrap_or("");
        let edit = match command {
            Command::Create => Edit::Create {
                account,
                identity: IdentityId::from_bytes(random()?),
                address,
                name: o.name.as_deref().unwrap_or(address),
                username: o.username.as_deref().unwrap_or(address),
            },
            Command::Update => Edit::Update {
                account,
                name: o.name.as_deref(),
                username: o.username.as_deref(),
            },
            Command::AliasAdd | Command::AliasRemove => Edit::Alias {
                account,
                address,
                remove: command == Command::AliasRemove,
            },
            _ => return Err(("arguments", "invalid-operation")),
        };
        let operation = with_addresses(&loaded, |request, deadline| {
            admin::prepare(request, &loaded.trust, deadline, epoch, &edit).map_err(classify)
        })?;
        save(path, &operation)
            .map_err(|_| ("operation-file", "write-or-sync-failed-no-database-writes"))?;
        operation
    };
    let receipt = with_addresses(&loaded, |request, deadline| {
        if command == Command::Inspect {
            admin::inspect(request, &loaded.trust, deadline, &operation).map_err(classify)
        } else {
            admin::apply(request, &loaded.trust, deadline, &operation)
                .map(Some)
                .map_err(classify)
        }
    })?;
    let receipt_json = match receipt {
        Some(r) => format!(
            "{{\"generation\":{},\"version\":\"{}\"}}",
            r.generation, r.version
        ),
        None => "null".to_owned(),
    };
    writeln!(io::stdout().lock(),"{{\"schema\":1,\"command\":\"{}\",\"status\":\"ok\",\"account\":\"{}\",\"version\":\"{}\",\"receipt\":{},\"receipt_is_historical\":true,\"restart_required\":{},\"profile_activated\":false}}",command.name(),operation.account(),operation.version(),receipt_json,receipt.is_some()).map_err(|_|("output","write-failed-retain-operation"))
}
fn epoch(
    loaded: &Loaded,
    provided: Option<StoreEpoch>,
) -> Result<StoreEpoch, (&'static str, &'static str)> {
    if let Some(epoch) = provided {
        return Ok(epoch);
    }
    with_addresses(loaded, |request, deadline| {
        postgres_schema::inspect(request, &loaded.trust, deadline)
            .map(|r| r.store_epoch)
            .map_err(AttemptError::from)
    })
}
fn list(loaded: &Loaded, provided: Option<StoreEpoch>) -> Result<(), (&'static str, &'static str)> {
    use td_mta::config::{
        stream,
        syntax::{Statement, Value},
    };
    let epoch = epoch(loaded, provided)?;
    let (selection, candidate) = with_addresses(loaded, |request, deadline| {
        postgres_selection::read(request, &loaded.trust, deadline, epoch)
            .map_err(|e| classify(admin::Error::Selection(e)))
    })?
    .ok_or(("account-admin", "no-selected-configuration"))?;
    let mut out = io::stdout().lock();
    write!(out,"{{\"schema\":1,\"command\":\"postgres-account-list\",\"status\":\"ok\",\"store_epoch\":\"{epoch}\",\"generation\":{},\"version\":\"{}\",\"records\":[",selection.generation,selection.version).map_err(|_|("output","write-failed"))?;
    let mut open = false;
    let mut first = true;
    let mut source = candidate.shared();
    let mut scratch = vec![0; stream::SCRATCH_BYTES];
    stream::read(&mut source, &mut scratch, |statement| -> io::Result<()> {
        match statement {
            Statement::Section { name, label, .. } => {
                if open {
                    write!(out, "}}")?;
                }
                open = matches!(name, "account" | "alias" | "identity" | "identity_address");
                if open {
                    if !first {
                        write!(out, ",")?;
                    }
                    first = false;
                    write!(
                        out,
                        "{{\"section\":{},\"id\":{}",
                        td_json::Json::Str(name.to_owned()),
                        td_json::Json::Str(label.unwrap_or("").to_owned())
                    )?;
                }
            }
            Statement::Assignment {
                key,
                value: Value::Text(value),
                ..
            } if open && matches!(key, "account" | "username" | "name" | "email" | "kind") => {
                write!(
                    out,
                    ",{}:{}",
                    td_json::Json::Str(key.to_owned()),
                    td_json::Json::Str(value.to_owned())
                )?
            }
            _ => {}
        }
        Ok(())
    })
    .map_err(|_| ("account-admin", "listing-failed"))?;
    if open {
        write!(out, "}}").map_err(|_| ("output", "write-failed"))?;
    }
    writeln!(out, "]}}").map_err(|_| ("output", "write-failed"))
}
pub(super) fn run(command: Command, args: impl Iterator<Item = OsString>) -> io::Result<ExitCode> {
    let Some(options) = arguments(command, args) else {
        report_error(command.name(), "arguments", "invalid-arguments")?;
        return Ok(ExitCode::from(2));
    };
    match execute(command, &options) {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err((stage, code)) => {
            report_error(command.name(), stage, code)?;
            Ok(ExitCode::FAILURE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn commands_refuse_ambiguous_or_inapplicable_options() {
        let base = [
            "--connection-file",
            "/private/connection",
            "--operation-file",
            "/private/op",
        ];
        assert!(arguments(Command::Apply, base.map(OsString::from).into_iter()).is_some());
        assert!(arguments(Command::Create, base.map(OsString::from).into_iter()).is_none());
        assert!(arguments(Command::List, base.map(OsString::from).into_iter()).is_none());
        assert!(arguments(
            Command::Apply,
            base.into_iter()
                .chain(["--name", "new"])
                .map(OsString::from)
        )
        .is_none());
        assert!(arguments(
            Command::Apply,
            base.into_iter()
                .chain(["--operation-file", "/private/second"])
                .map(OsString::from)
        )
        .is_none());
    }
}
