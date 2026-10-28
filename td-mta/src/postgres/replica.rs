//! Explicit bounded replica setup, copy and historical status; no listeners.
use super::{report_error, with_addresses, with_addresses_timeout, AttemptError};
use std::{
    ffi::{OsStr, OsString},
    io::{self, Write},
    process::ExitCode,
    sync::Arc,
    time::{Duration, Instant},
};
use td_mta::{
    ids::{AccountId, StoreEpoch},
    outbound_credentials,
    outbound_http::Client,
    postgres_input::Loaded,
    postgres_probe::{Mode, Request},
    postgres_replica_accounts,
    postgres_replica_progress::{self, Progress},
    postgres_replica_source::{self, Source},
    replica_copy::{Local, Remote, Worker},
    replica_seed::{Limits, Seed},
    replica_source::{Binding, Bounds, ReadLimits, Reader},
};

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Operation {
    Discover,
    Seed,
    Copy,
    Inspect,
}
impl Operation {
    pub(super) fn parse(value: &OsStr) -> Option<Self> {
        match value.to_str()? {
            "replica-discover" => Some(Self::Discover),
            "replica-account-init" => Some(Self::Seed),
            "replica-copy" => Some(Self::Copy),
            "replica-progress" => Some(Self::Inspect),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Discover => "postgres-replica-discover",
            Self::Seed => "postgres-replica-account-init",
            Self::Copy => "postgres-replica-copy",
            Self::Inspect => "postgres-replica-progress",
        }
    }
}
struct Options {
    connection: String,
    epoch: StoreEpoch,
    account: AccountId,
    source: Option<Source>,
    username: Option<String>,
    password: Option<String>,
    ca: Option<String>,
    limits: Option<Limits>,
    steps: usize,
    timeout: u64,
}
fn arguments(op: Operation, mut args: impl Iterator<Item = OsString>) -> Option<Options> {
    let mut values = std::collections::BTreeMap::new();
    while let Some(flag) = args.next() {
        let flag = flag.into_string().ok()?;
        let allowed = match flag.as_str() {
            "--connection-file" | "--epoch" | "--account" => true,
            "--source-origin" | "--source-epoch" | "--username" | "--password-file"
            | "--ca-file" => op != Operation::Inspect,
            "--body-bytes" | "--blobs" | "--emails" | "--receipts" => op == Operation::Seed,
            "--steps" | "--timeout-seconds" => op == Operation::Copy,
            _ => false,
        };
        if !allowed
            || values
                .insert(flag, args.next()?.into_string().ok()?)
                .is_some()
        {
            return None;
        }
    }
    let get = |key| values.get(key).map(String::as_str);
    let source = if op == Operation::Inspect {
        None
    } else {
        Some(
            Source::new(
                get("--source-origin")?,
                StoreEpoch::parse(get("--source-epoch")?).ok()?,
            )
            .ok()?,
        )
    };
    let limits = if op == Operation::Seed {
        let limits = Limits {
            body_bytes: number(get("--body-bytes")?)?,
            blobs: number(get("--blobs")?)?,
            emails: number(get("--emails")?)?,
            receipts: number(get("--receipts")?)?,
        };
        limits.validate().ok()?;
        Some(limits)
    } else {
        None
    };
    let steps = number(get("--steps").unwrap_or("1"))?;
    let timeout = number(get("--timeout-seconds").unwrap_or("60"))?;
    if !(1..=1024).contains(&steps) || !(1..=3600).contains(&timeout) {
        return None;
    }
    let username = if op == Operation::Inspect {
        None
    } else {
        Some(get("--username")?.to_owned())
    };
    let password = if op == Operation::Inspect {
        None
    } else {
        Some(get("--password-file")?.to_owned())
    };
    Some(Options {
        connection: get("--connection-file")?.to_owned(),
        epoch: StoreEpoch::parse(get("--epoch")?).ok()?,
        account: AccountId::parse(get("--account")?).ok()?,
        source,
        username,
        password,
        ca: get("--ca-file").map(str::to_owned),
        limits,
        steps: usize::try_from(steps).ok()?,
        timeout,
    })
}
fn number(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|v| v.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}
fn fail(code: &'static str) -> AttemptError {
    AttemptError::Failed("replica", code)
}
fn read_limits() -> ReadLimits {
    ReadLimits {
        json_bytes: 8 * 1024 * 1024,
        json_nodes: 262144,
        email: Bounds {
            body_bytes: td_mta::postgres_body::MAX_BODY_BYTES,
            values_per_email: 256,
        },
    }
}
pub(super) fn run(op: Operation, args: impl Iterator<Item = OsString>) -> io::Result<ExitCode> {
    let name = op.name();
    if op == Operation::Discover {
        return discover(args);
    }
    let Some(options) = arguments(op, args) else {
        report_error(name, "arguments", "invalid-arguments")?;
        return Ok(ExitCode::from(2));
    };
    let loaded = match Loaded::open(&options.connection, "replica") {
        Ok(value) => value,
        Err(error) => {
            report_error(name, "bootstrap", error.code())?;
            return Ok(ExitCode::FAILURE);
        }
    };
    if op == Operation::Inspect {
        let result = with_addresses(&loaded, |request, end| {
            postgres_replica_progress::inspect(
                request,
                &loaded.trust,
                end,
                options.epoch,
                options.account,
            )
            .map_err(progress_error)
        });
        return match result {
            Ok(progress) => {
                print_progress(name, options.epoch, &progress)?;
                Ok(ExitCode::SUCCESS)
            }
            Err((stage, code)) => {
                report_error(name, stage, code)?;
                Ok(ExitCode::FAILURE)
            }
        };
    }
    let Some(source) = options.source.as_ref() else {
        report_error(name, "arguments", "source-required")?;
        return Ok(ExitCode::from(2));
    };
    let credentials = match outbound_credentials::Loaded::open(
        options.username.as_deref().unwrap_or(""),
        options.password.as_deref().unwrap_or(""),
        options.ca.as_deref(),
    ) {
        Ok(value) => value,
        Err(code) => {
            report_error(name, "source-credentials", code)?;
            return Ok(ExitCode::FAILURE);
        }
    };
    let executable = std::env::current_exe()?;
    let result = with_addresses_timeout(
        &loaded,
        Duration::from_secs(options.timeout),
        |request, end| {
            if op == Operation::Seed {
                let bound = postgres_replica_source::inspect(
                    request,
                    &loaded.trust,
                    end.min(Instant::now() + Duration::from_secs(10)),
                    options.epoch,
                )
                .map_err(source_error)?;
                if bound.as_ref() != Some(source) {
                    return Err(fail("source-mismatch"));
                }
                // Existing persisted state is reported, never reseeded from a new observation.
                match postgres_replica_progress::inspect(
                    request,
                    &loaded.trust,
                    end.min(Instant::now() + Duration::from_secs(10)),
                    options.epoch,
                    options.account,
                ) {
                    Ok(progress) => {
                        if Some(progress.limits) != options.limits {
                            return Err(fail("limits-mismatch"));
                        }
                        return Ok(Some(progress));
                    }
                    Err(postgres_replica_progress::Error::Missing) => {}
                    Err(error) => return Err(progress_error(error)),
                }
                let mut reader = Reader::connect(
                    Client {
                        executable: &executable,
                        origin: source.origin(),
                        trust: &credentials.trust,
                        deadline: end.min(Instant::now() + Duration::from_secs(60)),
                    },
                    &credentials.authorization,
                    Some(Binding {
                        account: options.account,
                        epoch: source.epoch(),
                    }),
                    read_limits(),
                )
                .map_err(|_| fail("source-discovery-failed"))?;
                let inventory = reader
                    .inventory(None)
                    .map_err(|_| fail("source-inventory-failed"))?;
                let folders = reader
                    .mailboxes()
                    .map_err(|_| fail("source-mailboxes-failed"))?;
                let seed = Seed::prepare(
                    Source::new(source.origin(), source.epoch())
                        .map_err(|_| fail("source-invalid"))?,
                    inventory,
                    folders,
                    options.limits.ok_or_else(|| fail("limits-required"))?,
                )
                .map_err(|_| fail("seed-invalid"))?;
                postgres_replica_accounts::initialize(
                    request,
                    &loaded.trust,
                    end.min(Instant::now() + Duration::from_secs(10)),
                    options.epoch,
                    Arc::new(seed),
                )
                .map_err(|error| match error {
                    postgres_replica_accounts::Error::Indeterminate => {
                        fail("indeterminate-inspect-replica-progress")
                    }
                    _ => fail("account-initialization-failed"),
                })?;
                return postgres_replica_progress::inspect(
                    request,
                    &loaded.trust,
                    end.min(Instant::now() + Duration::from_secs(10)),
                    options.epoch,
                    options.account,
                )
                .map(Some)
                .map_err(|error| no_retry(progress_error(error)));
            }
            let runtime = Request {
                mode: Mode::Runtime,
                ..*request
            };
            let mut worker = Worker::new(
                Source::new(source.origin(), source.epoch()).map_err(|_| fail("source-invalid"))?,
                options.account,
                options.epoch,
                Local {
                    request: &runtime,
                    trust: &loaded.trust,
                },
            )
            .map_err(|_| fail("invalid-binding"))?;
            for index in 0..options.steps {
                let outcome = worker
                    .step(Remote {
                        client: Client {
                            executable: &executable,
                            origin: source.origin(),
                            trust: &credentials.trust,
                            deadline: end.min(Instant::now() + Duration::from_secs(60)),
                        },
                        authorization: &credentials.authorization,
                        limits: read_limits(),
                    })
                    .map_err(copy_error)?;
                // A blocked output consumer must not retain the migration lock.
                worker.retire();
                writeln!(io::stdout().lock(), "{{\"schema\":1,\"command\":\"{name}\",\"status\":\"ok\",\"step\":{},\"applied\":{},\"changed\":{},\"downloaded_bytes\":{},\"advanced\":{},\"empty_email_changes\":{},\"pending_mailbox_removals\":{},\"local_sequence\":{},\"copy_complete\":false,\"recovery_verified\":false}}", index+1, outcome.applied, outcome.changed, outcome.downloaded_bytes, outcome.advanced, outcome.empty_changes.is_some(), outcome.pending_mailbox_removals, outcome.local_sequence).map_err(|_| fail("status-write-failed"))?;
                if outcome.empty_changes.is_some() {
                    break;
                }
            }
            worker.retire();
            Ok(None)
        },
    );
    match result {
        Ok(Some(progress)) => {
            print_progress(name, options.epoch, &progress)?;
            Ok(ExitCode::SUCCESS)
        }
        Ok(None) => Ok(ExitCode::SUCCESS),
        Err((stage, code)) => {
            report_error(name, stage, code)?;
            Ok(ExitCode::FAILURE)
        }
    }
}
struct DiscoveryOptions {
    origin: String,
    username: String,
    password: String,
    ca: Option<String>,
}
fn discovery_arguments(mut args: impl Iterator<Item = OsString>) -> Option<DiscoveryOptions> {
    let mut values = std::collections::BTreeMap::new();
    while let Some(flag) = args.next() {
        let flag = flag.into_string().ok()?;
        if !matches!(
            flag.as_str(),
            "--source-origin" | "--username" | "--password-file" | "--ca-file"
        ) || values
            .insert(flag, args.next()?.into_string().ok()?)
            .is_some()
        {
            return None;
        }
    }
    let origin = values.remove("--source-origin")?;
    td_mta::config::endpoint::Origin::parse(&origin).ok()?;
    Some(DiscoveryOptions {
        origin,
        username: values.remove("--username")?,
        password: values.remove("--password-file")?,
        ca: values.remove("--ca-file"),
    })
}
fn discover(args: impl Iterator<Item = OsString>) -> io::Result<ExitCode> {
    let name = Operation::Discover.name();
    let Some(options) = discovery_arguments(args) else {
        report_error(name, "arguments", "invalid-arguments")?;
        return Ok(ExitCode::from(2));
    };
    let credentials = match outbound_credentials::Loaded::open(
        &options.username,
        &options.password,
        options.ca.as_deref(),
    ) {
        Ok(value) => value,
        Err(code) => {
            report_error(name, "source-credentials", code)?;
            return Ok(ExitCode::FAILURE);
        }
    };
    let executable = std::env::current_exe()?;
    let result = (|| {
        let mut reader = Reader::connect(
            Client {
                executable: &executable,
                origin: &options.origin,
                trust: &credentials.trust,
                deadline: Instant::now() + Duration::from_secs(60),
            },
            &credentials.authorization,
            None,
            read_limits(),
        )
        .map_err(|_| "source-discovery-failed")?;
        reader
            .inventory(None)
            .map_err(|_| "source-inventory-failed")?;
        let binding = reader.binding().ok_or("source-binding-missing")?;
        let source = Source::new(&options.origin, binding.epoch).map_err(|_| "source-invalid")?;
        Ok::<_, &str>((source, binding.account))
    })();
    match result {
        Ok((source, account)) => {
            writeln!(io::stdout().lock(), "{{\"schema\":1,\"command\":\"{name}\",\"status\":\"ok\",\"source_origin\":\"{}\",\"source_epoch\":\"{}\",\"account\":\"{account}\",\"copy_complete\":false,\"recovery_verified\":false}}", source.origin(), source.epoch())?;
            Ok(ExitCode::SUCCESS)
        }
        Err(code) => {
            report_error(name, "source", code)?;
            Ok(ExitCode::FAILURE)
        }
    }
}
fn no_retry(error: AttemptError) -> AttemptError {
    match error {
        AttemptError::Transport => fail("connection-failed"),
        other => other,
    }
}
fn source_error(error: postgres_replica_source::Error) -> AttemptError {
    use td_mta::{postgres_probe, postgres_schema};
    match error {
        postgres_replica_source::Error::Connection(postgres_probe::Error::Transport)
        | postgres_replica_source::Error::Schema(postgres_schema::Error::Connection(
            postgres_probe::Error::Transport,
        )) => AttemptError::Transport,
        _ => fail("source-inspection-failed"),
    }
}
fn progress_error(error: postgres_replica_progress::Error) -> AttemptError {
    use postgres_replica_progress::Error;
    if matches!(
        error,
        Error::Connection(td_mta::postgres_probe::Error::Transport)
            | Error::Schema(td_mta::postgres_schema::Error::Connection(
                td_mta::postgres_probe::Error::Transport
            ))
    ) {
        return AttemptError::Transport;
    }
    fail(match error {
        Error::Connection(_) => "connection-failed",
        Error::Schema(_) => "schema-mismatch",
        Error::Owner => "owner-required",
        Error::Epoch => "epoch-mismatch",
        Error::Source => "source-mismatch",
        Error::Missing => "account-not-initialized",
        Error::Integrity => "progress-integrity",
        Error::Capacity => "capacity",
        Error::Database => "database-failed",
    })
}
fn copy_error(error: td_mta::replica_copy::Error) -> AttemptError {
    use td_mta::replica_copy::Error;
    fail(match error {
        Error::Invalid => "invalid-binding",
        Error::Capacity => "capacity",
        Error::Deadline => "deadline",
        Error::Source(td_mta::replica_source::ReadError::Source(
            td_mta::replica_source::Error::Reset,
        )) => "source-history-expired",
        Error::Source(td_mta::replica_source::ReadError::Source(
            td_mta::replica_source::Error::SourceChanged,
        )) => "source-epoch-changed",
        Error::Source(td_mta::replica_source::ReadError::Capacity) => "source-response-capacity",
        Error::Source(td_mta::replica_source::ReadError::Integrity) => "source-body-integrity",
        Error::Source(td_mta::replica_source::ReadError::Deadline) => "source-deadline",
        Error::Source(td_mta::replica_source::ReadError::Transport(
            td_mta::outbound_http::Error::Deadline,
        )) => "source-deadline",
        Error::Source(td_mta::replica_source::ReadError::Transport(
            td_mta::outbound_http::Error::Status(401 | 403),
        )) => "source-authentication-refused",
        Error::Source(td_mta::replica_source::ReadError::Transport(
            td_mta::outbound_http::Error::Status(429 | 503),
        )) => "source-busy",
        Error::Source(_) => "source-read-failed",
        Error::Storage(td_mta::postgres_replica_apply::Error::Indeterminate) => {
            "indeterminate-reload-progress"
        }
        Error::Storage(td_mta::postgres_replica_apply::Error::Quota) => "quota",
        Error::Storage(td_mta::postgres_replica_apply::Error::Capacity) => "capacity",
        Error::Storage(_) => "publication-failed",
        Error::Effect(_) => "observation-invalid",
        Error::Graph(_) => "mailbox-graph-invalid",
        Error::Transition(_) => "page-transition-invalid",
    })
}
fn print_progress(name: &str, epoch: StoreEpoch, p: &Progress) -> io::Result<()> {
    let pending = p.pending.as_ref();
    writeln!(io::stdout().lock(), "{{\"schema\":1,\"command\":\"{name}\",\"status\":\"ok\",\"store_epoch\":\"{epoch}\",\"account\":\"{}\",\"source_origin\":\"{}\",\"source_epoch\":\"{}\",\"generation\":{},\"inventory_complete\":{},\"email_cursor\":{},\"mailbox_checkpoint\":{},\"email_count\":{},\"receipt_count\":{},\"pending_ids\":{},\"unfinished_ids\":{},\"copy_complete\":false,\"recovery_verified\":false}}", p.account, p.source.origin(), p.source.epoch(), p.generation, p.inventory_complete, p.email_cursor.sequence.number(), p.mailbox_checkpoint.sequence.number(), p.email_count, p.receipt_count, pending.map_or(0, |page| page.ids().len()), pending.map_or(0, |page| page.unfinished().count()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_requires_only_remote_scope_and_rejects_ambiguous_options() {
        let base = [
            "--source-origin",
            "https://mail.example.test",
            "--username",
            "user",
            "--password-file",
            "private/password",
        ];
        let parse =
            |extra: &[&str]| discovery_arguments(base.iter().chain(extra).map(OsString::from));
        assert!(parse(&[]).is_some());
        assert!(parse(&["--ca-file", "private/ca"]).is_some());
        for extra in [
            vec!["--connection-file", "local"],
            vec!["--account", "id"],
            vec!["--password", "secret"],
            vec!["--username", "other"],
            vec!["--ca-file"],
        ] {
            assert!(parse(&extra).is_none());
        }
        assert!(discovery_arguments(
            [
                "--source-origin",
                "http://mail.example.test",
                "--username",
                "user",
                "--password-file",
                "private/password"
            ]
            .map(OsString::from)
            .into_iter()
        )
        .is_none());
        assert!(discovery_arguments(
            ["--source-origin", "https://mail.example.test"]
                .map(OsString::from)
                .into_iter()
        )
        .is_none());
    }
    #[test]
    fn address_retry_is_limited_to_read_only_preflights() {
        let transport = td_mta::postgres_probe::Error::Transport;
        assert!(matches!(
            source_error(postgres_replica_source::Error::Connection(transport)),
            AttemptError::Transport
        ));
        assert!(matches!(
            progress_error(postgres_replica_progress::Error::Connection(transport)),
            AttemptError::Transport
        ));
        assert!(matches!(
            no_retry(progress_error(
                postgres_replica_progress::Error::Connection(transport)
            )),
            AttemptError::Failed(_, "connection-failed")
        ));
        assert!(matches!(
            copy_error(td_mta::replica_copy::Error::Storage(
                td_mta::postgres_replica_apply::Error::Connection(transport)
            )),
            AttemptError::Failed(_, _)
        ));
    }
    #[test]
    fn operator_commands_require_scope_and_refuse_unbounded_or_ambiguous_options() {
        let base = [
            "--connection-file",
            "private",
            "--epoch",
            "11111111111111111111111111111111",
            "--account",
            "22222222222222222222222222222222",
        ];
        let remote = [
            "--source-origin",
            "https://mail.example.test",
            "--source-epoch",
            "33333333333333333333333333333333",
            "--username",
            "user",
            "--password-file",
            "private/password",
        ];
        let limits = [
            "--body-bytes",
            "1048576",
            "--blobs",
            "10",
            "--emails",
            "10",
            "--receipts",
            "100",
        ];
        let parse = |op, parts: &[&[&str]]| {
            arguments(
                op,
                base.iter()
                    .copied()
                    .chain(parts.iter().flat_map(|part| part.iter().copied()))
                    .map(OsString::from),
            )
        };
        assert!(parse(Operation::Inspect, &[]).is_some());
        assert!(parse(Operation::Copy, &[&remote]).is_some());
        assert!(parse(Operation::Seed, &[&remote, &limits]).is_some());
        assert!(parse(Operation::Seed, &[&remote]).is_none());
        assert!(parse(Operation::Copy, &[]).is_none());
        assert!(parse(Operation::Inspect, &[&remote]).is_none());
        for invalid in [
            vec!["--password", "secret"],
            vec!["--steps", "0"],
            vec!["--steps", "1025"],
            vec!["--timeout-seconds", "3601"],
            vec!["--timeout-seconds", "-1"],
            vec!["--epoch", "11111111111111111111111111111111"],
        ] {
            assert!(parse(Operation::Copy, &[&remote, &invalid]).is_none());
        }
        assert!(parse(
            Operation::Seed,
            &[
                &remote,
                &[
                    "--body-bytes",
                    "0",
                    "--blobs",
                    "1",
                    "--emails",
                    "1",
                    "--receipts",
                    "1"
                ]
            ]
        )
        .is_none());
    }
}
