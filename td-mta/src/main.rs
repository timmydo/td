//! Offline account verification, database backup and packaging entry point.
#![forbid(unsafe_code)]

use std::{
    ffi::OsStr,
    io::{self, Write},
    process::ExitCode,
    sync::Arc,
};
use td_mta::{
    account_checks::CompleteChecks,
    clock::RuntimeClock,
    ids::{AccountId, StoreEpoch},
    limits::SQLITE_DATABASE_BYTES,
    metadata_sweep,
    ports::{self, Clock, Deadline},
    store_fs::{
        AccountCheckError, AccountCheckLimits, BackupError, BackupReceipt, BodyCheckLimits,
        IndexStore, LockError, LockedRoot, PrivateRoot,
    },
};

const HELP: &str = "Usage: td-mta --version | --help\n       td-mta store verify --root PATH (--account ID | --all) [--timeout-seconds N]\n       td-mta backup --root PATH --destination PATH [--timeout-seconds N]\n\nVerify database integrity and metadata/body digests for selected or all accounts.\nRun offline with access to the private store; the writer lock must be free.\nVerification JSON identifies its account or database scope. Default timeout: 600 seconds.\nBackup copies the stopped database to an existing private destination root.\nA completed copy does not certify semantic integrity or enable restore.\nNo repair, restore or serving commands are available.\n";

enum Selection {
    Account(AccountId),
    All,
}

struct Verify {
    root: String,
    selection: Selection,
    timeout_ms: u64,
}

fn arguments(mut args: impl Iterator<Item = std::ffi::OsString>) -> Option<Verify> {
    let mut root = None;
    let mut selection = None;
    let mut timeout = None;
    while let Some(flag) = args.next() {
        match flag.to_str()? {
            "--root" if root.is_none() => root = Some(args.next()?.into_string().ok()?),
            "--account" if selection.is_none() => {
                selection = Some(Selection::Account(
                    AccountId::parse(args.next()?.to_str()?).ok()?,
                ));
            }
            "--all" if selection.is_none() => selection = Some(Selection::All),
            "--timeout-seconds" if timeout.is_none() => {
                timeout = Some(timeout_ms(args.next()?.to_str()?)?);
            }
            _ => return None,
        }
    }
    Some(Verify {
        root: root?,
        selection: selection?,
        timeout_ms: timeout.unwrap_or(600_000),
    })
}

struct Backup {
    root: String,
    destination: String,
    timeout_ms: u64,
}

fn timeout_ms(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let seconds = value.parse::<u64>().ok()?;
    if seconds == 0 {
        return None;
    }
    seconds.checked_mul(1000)
}

fn backup_arguments(mut args: impl Iterator<Item = std::ffi::OsString>) -> Option<Backup> {
    let mut root = None;
    let mut destination = None;
    let mut timeout = None;
    while let Some(flag) = args.next() {
        let value = args.next()?.into_string().ok()?;
        match flag.to_str()? {
            "--root" if root.is_none() => root = Some(value),
            "--destination" if destination.is_none() => destination = Some(value),
            "--timeout-seconds" if timeout.is_none() => timeout = Some(timeout_ms(&value)?),
            _ => return None,
        }
    }
    Some(Backup {
        root: root?,
        destination: destination?,
        timeout_ms: timeout.unwrap_or(600_000),
    })
}

struct Failure {
    stage: &'static str,
    code: &'static str,
}

impl Failure {
    fn exit_code(&self) -> ExitCode {
        if self.stage == "arguments" {
            ExitCode::from(2)
        } else {
            ExitCode::FAILURE
        }
    }
}

fn adapter(stage: &'static str, error: ports::Error) -> Failure {
    let code = match error {
        ports::Error::Capacity => "capacity",
        ports::Error::Quota => "quota",
        ports::Error::Busy => "busy",
        ports::Error::Deadline => "deadline",
        ports::Error::NotFound => "not-found",
        ports::Error::Forbidden => "forbidden",
        ports::Error::Invalid => "invalid",
        ports::Error::Corrupt => "corrupt",
        ports::Error::Conflict => "conflict",
        ports::Error::HistoryLost => "history-lost",
        ports::Error::WriterStopped => "writer-stopped",
        ports::Error::Entropy => "entropy",
        ports::Error::Crypto => "crypto",
        ports::Error::Tls => "tls",
        ports::Error::Dns => "dns",
        ports::Error::Io { .. } => "io",
    };
    Failure { stage, code }
}

fn scope(clock: &dyn Clock, timeout_ms: u64) -> Result<Deadline, Failure> {
    let started = clock.sample().map_err(|error| adapter("clock", error))?;
    Deadline::after(started.monotonic, timeout_ms).map_err(|_| Failure {
        stage: "arguments",
        code: "invalid-arguments",
    })
}

fn locked_root(
    path: &str,
    root_stage: &'static str,
    lock_stage: &'static str,
) -> Result<LockedRoot, Failure> {
    let root = PrivateRoot::open(path).map_err(|_| Failure {
        stage: root_stage,
        code: "root-policy-or-io",
    })?;
    root.try_lock().map_err(|error| Failure {
        stage: lock_stage,
        code: match error {
            LockError::Busy => "busy",
            LockError::Policy => "lock-policy",
            LockError::Io(_) => "io",
        },
    })
}

struct BackupFailure {
    failure: Failure,
    publication: &'static str,
}
impl From<Failure> for BackupFailure {
    fn from(failure: Failure) -> Self {
        Self {
            failure,
            publication: "unpublished",
        }
    }
}

fn backup(options: Backup) -> Result<BackupReceipt, BackupFailure> {
    let clock: Arc<dyn Clock> = Arc::new(RuntimeClock::new());
    let deadline = scope(clock.as_ref(), options.timeout_ms)?;
    let mut source = locked_root(&options.root, "source-root", "source-lock")?;
    let mut destination =
        locked_root(&options.destination, "destination-root", "destination-lock")?;
    let store = IndexStore::open(&mut source, clock, 1, deadline)
        .map_err(|error| adapter("source-open", error))?;
    store
        .backup(&mut destination, deadline, &mut [0; 65536])
        .map_err(|error| match error {
            BackupError::Unpublished(error) => BackupFailure {
                failure: adapter("copy", error),
                publication: "unpublished",
            },
            BackupError::IncompletePublication(error) => BackupFailure {
                failure: adapter("publication", error),
                publication: "uncertain",
            },
        })
}

fn backup_failure(output: &mut impl Write, error: BackupFailure) -> io::Result<()> {
    writeln!(output, "{{\"schema\":1,\"command\":\"backup\",\"status\":\"error\",\"stage\":\"{}\",\"error\":\"{}\",\"publication\":\"{}\"}}", error.failure.stage, error.failure.code, error.publication)
}

fn verify(options: Verify) -> Result<Verification, Failure> {
    let clock: Arc<dyn Clock> = Arc::new(RuntimeClock::new());
    verify_with_clock(options, clock)
}

fn verify_with_clock(options: Verify, clock: Arc<dyn Clock>) -> Result<Verification, Failure> {
    let deadline = scope(clock.as_ref(), options.timeout_ms)?;
    let mut root = locked_root(&options.root, "root", "lock")?;
    let store = IndexStore::open(&mut root, Arc::clone(&clock), 1, deadline)
        .map_err(|error| adapter("open", error))?;
    store
        .validate_integrity(deadline)
        .map_err(|error| adapter("physical", error))?;
    let mut scratch = [0; 65536];
    match options.selection {
        Selection::Account(account) => {
            check_account(&store, account, clock.as_ref(), deadline, &mut scratch)
                .map(|report| Verification::Account(Box::new(report)))
        }
        Selection::All => {
            let accounts = store
                .account_ids(deadline)
                .map_err(|error| adapter("accounts", error))?;
            let mut report = DatabaseChecks {
                epoch: store.epoch(),
                checked_at_ms: 0,
                accounts: 0,
                metadata_rows: 0,
                mailboxes: 0,
                submissions: 0,
                recipients: 0,
                blobs: 0,
                body_bytes: 0,
            };
            for account in accounts {
                let checked =
                    check_account(&store, account, clock.as_ref(), deadline, &mut scratch)?;
                report.add(checked)?;
            }
            report.checked_at_ms = clock
                .sample()
                .map_err(|error| adapter("clock", error))?
                .utc_ms;
            Ok(Verification::Database(report))
        }
    }
}

enum Verification {
    Account(Box<CompleteChecks>),
    Database(DatabaseChecks),
}

struct DatabaseChecks {
    epoch: StoreEpoch,
    checked_at_ms: i64,
    accounts: u64,
    metadata_rows: u64,
    mailboxes: u64,
    submissions: u64,
    recipients: u64,
    blobs: u64,
    body_bytes: u64,
}
impl DatabaseChecks {
    fn add(&mut self, checked: CompleteChecks) -> Result<(), Failure> {
        if checked.identity().epoch != self.epoch {
            return Err(adapter("reports", ports::Error::Corrupt));
        }
        let metadata = checked.metadata();
        let bodies = checked.bodies();
        for (total, count) in [
            (&mut self.accounts, 1),
            (&mut self.metadata_rows, metadata.references().rows()),
            (&mut self.mailboxes, metadata.mailboxes().mailboxes()),
            (&mut self.submissions, metadata.recipients().submissions()),
            (&mut self.recipients, metadata.recipients().recipients()),
            (&mut self.blobs, bodies.blobs()),
            (&mut self.body_bytes, bodies.bytes()),
        ] {
            *total = total
                .checked_add(count)
                .ok_or_else(|| adapter("reports", ports::Error::Capacity))?;
        }
        if self.body_bytes > SQLITE_DATABASE_BYTES {
            return Err(adapter("reports", ports::Error::Capacity));
        }
        Ok(())
    }
}

fn check_account(
    store: &IndexStore<'_>,
    account: AccountId,
    clock: &dyn Clock,
    deadline: Deadline,
    scratch: &mut [u8; 65536],
) -> Result<CompleteChecks, Failure> {
    let mut view = store
        .maintenance_view(account, deadline)
        .map_err(|error| adapter("account", error))?;
    let utc_ms = clock
        .sample()
        .map_err(|error| adapter("clock", error))?
        .utc_ms;
    // Each captured account view retains one allowance across all its passes.
    let limits = AccountCheckLimits {
        metadata: metadata_sweep::Limits {
            rows: u64::MAX,
            parent_reads: u64::MAX,
        },
        bodies: BodyCheckLimits {
            blobs: u64::MAX,
            bytes: SQLITE_DATABASE_BYTES,
        },
    };
    view.verify_account(&td_crypto::Provider, utc_ms, limits, scratch)
        .map_err(|error| match error {
            AccountCheckError::Bodies(error) => adapter("bodies", error),
            AccountCheckError::Metadata(_) => Failure {
                stage: "metadata",
                code: "verification-failed",
            },
            AccountCheckError::Reports(_) => Failure {
                stage: "reports",
                code: "verification-failed",
            },
        })
}

fn failure(output: &mut impl Write, command: &str, error: Failure) -> io::Result<()> {
    writeln!(output, "{{\"schema\":1,\"command\":\"{}\",\"status\":\"error\",\"stage\":\"{}\",\"error\":\"{}\"}}", command, error.stage, error.code)
}

fn run() -> io::Result<ExitCode> {
    let mut args = std::env::args_os();
    let _ = args.next();
    let first = args.next();
    if first.as_deref() == Some(OsStr::new("backup")) {
        let Some(options) = backup_arguments(args) else {
            backup_failure(
                &mut io::stdout().lock(),
                Failure {
                    stage: "arguments",
                    code: "invalid-arguments",
                }
                .into(),
            )?;
            return Ok(ExitCode::from(2));
        };
        return match backup(options) {
            Ok(receipt) => {
                writeln!(io::stdout().lock(), "{{\"schema\":1,\"command\":\"backup\",\"status\":\"ok\",\"scope\":\"database\",\"epoch\":\"{}\",\"bytes\":{},\"semantic_verified\":false}}", receipt.epoch, receipt.bytes)?;
                Ok(ExitCode::SUCCESS)
            }
            Err(error) => {
                let code = error.failure.exit_code();
                backup_failure(&mut io::stdout().lock(), error)?;
                Ok(code)
            }
        };
    }
    if first.as_deref() == Some(OsStr::new("store")) {
        if args.next().as_deref() != Some(OsStr::new("verify")) {
            failure(
                &mut io::stdout().lock(),
                "store",
                Failure {
                    stage: "arguments",
                    code: "invalid-arguments",
                },
            )?;
            return Ok(ExitCode::from(2));
        }
        let Some(options) = arguments(args) else {
            failure(
                &mut io::stdout().lock(),
                "store.verify",
                Failure {
                    stage: "arguments",
                    code: "invalid-arguments",
                },
            )?;
            return Ok(ExitCode::from(2));
        };
        return match verify(options) {
            Ok(Verification::Database(report)) => {
                writeln!(io::stdout().lock(), "{{\"schema\":1,\"command\":\"store.verify\",\"status\":\"ok\",\"scope\":\"database\",\"physical_integrity\":true,\"epoch\":\"{}\",\"checked_at_ms\":{},\"accounts\":{},\"metadata_rows\":{},\"mailboxes\":{},\"submissions\":{},\"recipients\":{},\"blobs\":{},\"body_bytes\":{}}}", report.epoch, report.checked_at_ms, report.accounts, report.metadata_rows, report.mailboxes, report.submissions, report.recipients, report.blobs, report.body_bytes)?;
                Ok(ExitCode::SUCCESS)
            }
            Ok(Verification::Account(report)) => {
                let identity = report.identity();
                let metadata = report.metadata();
                let bodies = report.bodies();
                writeln!(io::stdout().lock(), "{{\"schema\":1,\"command\":\"store.verify\",\"status\":\"ok\",\"scope\":\"account\",\"physical_integrity\":true,\"account\":\"{}\",\"epoch\":\"{}\",\"sequence\":{},\"history_floor\":{},\"checked_at_ms\":{},\"metadata_rows\":{},\"mailboxes\":{},\"submissions\":{},\"recipients\":{},\"blobs\":{},\"body_bytes\":{}}}", identity.account, identity.epoch, identity.committed_sequence.number(), identity.history_floor.number(), metadata.references().utc_ms(), metadata.references().rows(), metadata.mailboxes().mailboxes(), metadata.recipients().submissions(), metadata.recipients().recipients(), bodies.blobs(), bodies.bytes())?;
                Ok(ExitCode::SUCCESS)
            }
            Err(error) => {
                let code = error.exit_code();
                failure(&mut io::stdout().lock(), "store.verify", error)?;
                Ok(code)
            }
        };
    }
    let extra = args.next().is_some();
    if !extra && first.as_deref() == Some(OsStr::new("--version")) {
        writeln!(io::stdout().lock(), "td-mta {}", env!("CARGO_PKG_VERSION"))?;
        return Ok(ExitCode::SUCCESS);
    }
    if !extra && first.as_deref() == Some(OsStr::new("--help")) {
        io::stdout().lock().write_all(HELP.as_bytes())?;
        return Ok(ExitCode::SUCCESS);
    }
    io::stderr().lock().write_all(HELP.as_bytes())?;
    Ok(ExitCode::from(2))
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(_) => ExitCode::FAILURE,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use td_mta::ports::{Tick, Time};

    struct FixedClock(u64);
    impl Clock for FixedClock {
        fn sample(&self) -> Result<Time, ports::Error> {
            Ok(Time {
                monotonic: Tick(self.0),
                utc_ms: 17,
            })
        }
    }

    #[test]
    fn absolute_timeout_overflow_is_usage_before_root_access() {
        for (tick, stage, error_code, code) in [
            (615, "root", "root-policy-or-io", 1),
            (616, "arguments", "invalid-arguments", 2),
        ] {
            let error = verify_with_clock(
                Verify {
                    root: String::new(),
                    selection: Selection::Account(AccountId::from_bytes([1; 16])),
                    timeout_ms: 18_446_744_073_709_551_000,
                },
                Arc::new(FixedClock(tick)),
            )
            .err()
            .unwrap();
            assert_eq!(error.stage, stage);
            assert_eq!(error.code, error_code);
            assert_eq!(error.exit_code(), ExitCode::from(code));
        }
    }
}
