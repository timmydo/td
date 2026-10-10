//! Operator commands and foreground SMTP receiving.
#![forbid(unsafe_code)]

use std::{
    ffi::OsStr,
    io::{self, Write},
    process::ExitCode,
    sync::Arc,
};
use td_mta::{
    account_checks::CompleteChecks,
    clock::{RuntimeClock, TlsClockSource},
    config::{inputs::Target, load, materialize, preimage, stanza::Pending, storage, stream},
    generations::GenerationSet,
    ids::{AccountId, MailboxId, StoreEpoch},
    limits::SQLITE_DATABASE_BYTES,
    metadata_sweep,
    operator_files::{Inputs, Role},
    ports::{self, Clock, Deadline, Entropy as _},
    store_fs::{
        AccountCheckError, AccountCheckLimits, BackupError, BackupReceipt, BodyCheckLimits,
        IndexStore, LockError, LockedRoot, PrivateRoot,
    },
    tls_policy::{MaterialKind, TlsPolicies},
};

mod serve;

const HELP: &str = "Usage: td-mta --version | --help\n       td-mta config check --config PATH\n       td-mta serve --smtp-only --config PATH\n       td-mta store init --root PATH --account ID [--timeout-seconds N]\n       td-mta store verify --root PATH (--account ID | --all) [--timeout-seconds N]\n       td-mta backup --root PATH --destination PATH [--timeout-seconds N]\n       td-mta restore --root PATH --destination PATH [--timeout-seconds N]\n\nVerify database integrity and metadata/body digests for selected or all accounts.\nRun offline with access to the private store; the writer lock must be free.\nInit creates a store holding one account and its Inbox; it does not serve.\nVerification JSON identifies its account or database scope. Default timeout: 600 seconds.\nBackup copies the stopped database to an existing private destination root.\nA completed copy does not certify semantic integrity or enable restore.\nRestore verifies the copy and renews its epoch; it does not start service.\nConfig check opens the file and every referenced input under the protected-file\nrules and decodes text and TLS material; run it as the service user.\nACME-managed material is unavailable to it.\nServe requires an initialized data root, its private ingress/ subdirectory,\nand private runtime/log roots.\nIt receives direct IPv4 SMTP only; HTTPS, outbound delivery and reload are inactive.\nStop it through the supervisor; signals currently terminate without draining.\nNo repair command is available.\n";

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

struct CopyOptions {
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

fn copy_arguments(mut args: impl Iterator<Item = std::ffi::OsString>) -> Option<CopyOptions> {
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
    Some(CopyOptions {
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
    lock_root(root, lock_stage)
}

fn lock_root(root: PrivateRoot, stage: &'static str) -> Result<LockedRoot, Failure> {
    root.try_lock().map_err(|error| Failure {
        stage,
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

fn backup(options: CopyOptions) -> Result<BackupReceipt, BackupFailure> {
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

struct RestoreReceipt {
    copy: BackupReceipt,
    epoch: StoreEpoch,
    checks: DatabaseChecks,
}

struct RestoreFailure {
    failure: Failure,
    progress: &'static str,
}
impl From<Failure> for RestoreFailure {
    fn from(failure: Failure) -> Self {
        Self {
            failure,
            progress: "unpublished",
        }
    }
}

fn restore(options: CopyOptions) -> Result<RestoreReceipt, RestoreFailure> {
    let clock: Arc<dyn Clock> = Arc::new(RuntimeClock::new());
    let deadline = scope(clock.as_ref(), options.timeout_ms)?;
    let mut source = locked_root(&options.root, "source-root", "source-lock")?;
    let mut destination =
        locked_root(&options.destination, "destination-root", "destination-lock")?;
    let store = IndexStore::open(&mut source, Arc::clone(&clock), 1, deadline)
        .map_err(|error| adapter("source-open", error))?;
    let mut scratch = [0; 65536];
    let copy = store
        .backup(&mut destination, deadline, &mut scratch)
        .map_err(|error| match error {
            BackupError::Unpublished(error) => RestoreFailure {
                failure: adapter("copy", error),
                progress: "unpublished",
            },
            BackupError::IncompletePublication(error) => RestoreFailure {
                failure: adapter("publication", error),
                progress: "copy-uncertain",
            },
        })?;
    let copied = |failure| RestoreFailure {
        failure,
        progress: "copied",
    };
    let store = IndexStore::open(&mut destination, Arc::clone(&clock), 1, deadline)
        .map_err(|error| copied(adapter("destination-open", error)))?;
    let checks = check_database(&store, clock.as_ref(), deadline, &mut scratch).map_err(copied)?;
    if checks.epoch != copy.epoch {
        return Err(copied(adapter("reports", ports::Error::Corrupt)));
    }
    let mut entropy = td_crypto::SystemEntropy::try_new()
        .map_err(|error| copied(adapter("entropy", error.into())))?;
    let store = store
        .renew_epoch(&mut entropy, deadline)
        .map_err(|error| match error {
            ports::CommitFailure::Rejected(error) => copied(adapter("epoch", error)),
            ports::CommitFailure::Indeterminate(error) => RestoreFailure {
                failure: adapter("epoch", error),
                progress: "epoch-uncertain",
            },
        })?;
    let epoch = store.epoch();
    store.checkpoint(deadline).map_err(|error| RestoreFailure {
        failure: adapter("checkpoint", error),
        progress: "epoch-renewed",
    })?;
    drop(store);
    Ok(RestoreReceipt {
        copy,
        epoch,
        checks,
    })
}

fn restore_failure(output: &mut impl Write, error: RestoreFailure) -> io::Result<()> {
    writeln!(output, "{{\"schema\":1,\"command\":\"restore\",\"status\":\"error\",\"stage\":\"{}\",\"error\":\"{}\",\"progress\":\"{}\"}}", error.failure.stage, error.failure.code, error.progress)
}

struct InitReceipt {
    account: AccountId,
    epoch: StoreEpoch,
    inbox: MailboxId,
}

struct InitFailure {
    failure: Failure,
    progress: &'static str,
}
impl From<Failure> for InitFailure {
    fn from(failure: Failure) -> Self {
        Self {
            failure,
            progress: "unstarted",
        }
    }
}

fn init(options: Verify) -> Result<InitReceipt, InitFailure> {
    let Selection::Account(account) = options.selection else {
        return Err(Failure {
            stage: "arguments",
            code: "invalid-arguments",
        }
        .into());
    };
    let clock: Arc<dyn Clock> = Arc::new(RuntimeClock::new());
    let deadline = scope(clock.as_ref(), options.timeout_ms)?;
    // Both identities exist before the root is touched.
    let (mut epoch, mut inbox) = ([0; 16], [0; 16]);
    td_crypto::SystemEntropy::try_new()
        .and_then(|mut entropy| {
            entropy.fill(&mut epoch)?;
            entropy.fill(&mut inbox)
        })
        .map_err(|error| adapter("entropy", error.into()))?;
    let (epoch, inbox) = (StoreEpoch::from_bytes(epoch), MailboxId::from_bytes(inbox));
    let mut root = locked_root(&options.root, "root", "lock")?;
    let store = IndexStore::create_with_inbox(&mut root, epoch, account, inbox, clock, 1, deadline)
        .map_err(|error| match error {
            // The exclusive database open is creation's first file effect.
            ports::Error::Io {
                kind: io::ErrorKind::AlreadyExists,
                ..
            } => InitFailure {
                failure: Failure {
                    stage: "create",
                    code: "exists",
                },
                progress: "unstarted",
            },
            error => InitFailure {
                failure: adapter("create", error),
                progress: "uncertain",
            },
        })?;
    store.checkpoint(deadline).map_err(|error| InitFailure {
        failure: adapter("checkpoint", error),
        progress: "created",
    })?;
    drop(store);
    Ok(InitReceipt {
        account,
        epoch,
        inbox,
    })
}

fn init_failure(output: &mut impl Write, error: InitFailure) -> io::Result<()> {
    writeln!(output, "{{\"schema\":1,\"command\":\"store.init\",\"status\":\"error\",\"stage\":\"{}\",\"error\":\"{}\",\"progress\":\"{}\"}}", error.failure.stage, error.failure.code, error.progress)
}

fn config_arguments(mut args: impl Iterator<Item = std::ffi::OsString>) -> Option<String> {
    if args.next()?.to_str()? != "--config" {
        return None;
    }
    let path = args.next()?.into_string().ok()?;
    args.next().is_none().then_some(path)
}

/// A fixed-code refusal; detail is the loader's own path- and byte-free text.
struct ConfigFailure {
    failure: Failure,
    target: Option<&'static str>,
    detail: Option<String>,
}
impl From<Failure> for ConfigFailure {
    fn from(failure: Failure) -> Self {
        Self {
            failure,
            target: None,
            detail: None,
        }
    }
}
fn config_failure(stage: &'static str, code: &'static str) -> ConfigFailure {
    Failure { stage, code }.into()
}
fn input_failure(code: &'static str, target: &'static str) -> ConfigFailure {
    ConfigFailure {
        target: Some(target),
        ..config_failure("input", code)
    }
}

fn input_target(target: Target) -> &'static str {
    match target {
        Target::TextSignature(_) => "text-signature",
        Target::HtmlSignature(_) => "html-signature",
        Target::RelayPassword => "relay-password",
        Target::RelayCa => "relay-ca",
        Target::AcmeCa => "acme-ca",
        Target::CertificateChain(_) => "certificate-chain",
        Target::CertificateKey(_) => "certificate-key",
        Target::GatewayCa(_) => "gateway-ca",
    }
}
fn material_target(kind: MaterialKind) -> &'static str {
    match kind {
        MaterialKind::Chain => "certificate-chain",
        MaterialKind::Key => "certificate-key",
        MaterialKind::GatewayCa => "gateway-ca",
        MaterialKind::RelayCa => "relay-ca",
        MaterialKind::AcmeCa => "acme-ca",
    }
}

/// Every stage a service start needs before touching the store: protected
/// opening of the file and all references, structural load, text decoding,
/// TLS provider construction and identity encoding, retained for activation.
struct CheckedConfiguration {
    resolved: materialize::ResolvedText,
    prepared: td_mta::generations::PreparedGeneration<TlsPolicies>,
    generations: GenerationSet<TlsPolicies>,
    root: PrivateRoot,
    inputs: usize,
}

fn config_check(path: &str) -> Result<usize, ConfigFailure> {
    Ok(load_configuration(path, Arc::new(RuntimeClock::new()))?.inputs)
}

fn load_configuration(
    path: &str,
    clock: Arc<dyn Clock>,
) -> Result<CheckedConfiguration, ConfigFailure> {
    let mut files = Inputs::new();
    let mut source = files
        .open(path, Role::Configuration)
        .map_err(|error| config_failure("configuration-file", error.code()))?;
    let mut scratch = vec![0; stream::SCRATCH_BYTES];
    let storage =
        storage::Storage::try_new().map_err(|_| config_failure("configuration", "capacity"))?;
    let loaded =
        load::read(storage, &mut Pending::new(), &mut scratch, &mut source).map_err(|failure| {
            let (code, detail) = match failure.error() {
                storage::BuildError::Input(stream::Error::Handler(error))
                | storage::BuildError::Configuration(error) => ("config_schema", error.to_string()),
                storage::BuildError::Input(error) => (error.name(), error.to_string()),
            };
            ConfigFailure {
                detail: Some(detail),
                ..config_failure("configuration", code)
            }
        })?;
    drop(source);
    let root = loaded
        .candidate()
        .globals()
        .ok()
        .and_then(|globals| globals.data().ok())
        .ok_or_else(|| config_failure("configuration", "invariant"))
        .and_then(|data| {
            PrivateRoot::open(data).map_err(|_| config_failure("data-root", "root-policy-or-io"))
        })?;
    files
        .bind(&root)
        .map_err(|error| config_failure("data-root", error.code()))?;
    let resolved = materialize::read_text(loaded, &mut scratch, |reference| {
        files.open(reference.path(), Role::of(reference.target()))
    })
    .map_err(|failure| {
        let (stage, code) = match failure.error() {
            materialize::Error::Input(error) => ("input", error.code()),
            materialize::Error::Content(error) => ("input", error.name()),
            // Arena, inventory and invariant refusals are not one input's.
            error => ("text", error.name()),
        };
        ConfigFailure {
            target: failure.target().map(input_target),
            ..config_failure(stage, code)
        }
    })?;
    let tls_clock = Arc::new(td_crypto::ClockHandle::new(TlsClockSource::new(clock)));
    let set = GenerationSet::<TlsPolicies>::at_startup();
    let reserved = set.reserve().map_err(|error| adapter("tls", error))?;
    let (mut refused, mut last) = (None, None);
    // Publication belongs to serve; config check drops the prepared tables.
    let prepared = TlsPolicies::prepare(reserved, &resolved, tls_clock, |request| {
        let target = material_target(request.kind());
        last = Some(target);
        // ACME chains and keys are service state; this check reads no state.
        let Some(path) = request.path() else {
            refused = Some(input_failure("acme-material-unavailable", target));
            return Err(ports::Error::NotFound);
        };
        let role = if request.kind() == MaterialKind::Key {
            Role::Secret
        } else {
            Role::Public
        };
        files.open(path, role).map_err(|error| {
            refused = Some(input_failure(error.code(), target));
            ports::Error::Forbidden
        })
    })
    .map_err(|error| {
        refused.take().unwrap_or_else(|| ConfigFailure {
            // Content is parsed after reading; a key pair may fail on either.
            target: last,
            ..adapter("tls", error).into()
        })
    })?;
    preimage::write(&resolved, |_| Ok::<(), std::convert::Infallible>(()))
        .map_err(|error| config_failure("identities", error.name()))?;
    Ok(CheckedConfiguration {
        resolved,
        prepared,
        generations: set,
        root,
        inputs: files.count(),
    })
}

fn json_text(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            c if c.is_control() => output.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => output.push(c),
        }
    }
    output
}

fn config_check_failure(output: &mut impl Write, error: ConfigFailure) -> io::Result<()> {
    configuration_failure(output, "config.check", error)
}
fn configuration_failure(
    output: &mut impl Write,
    command: &str,
    error: ConfigFailure,
) -> io::Result<()> {
    write!(
        output,
        "{{\"schema\":1,\"command\":\"{command}\",\"status\":\"error\",\"stage\":\"{}\",\"error\":\"{}\"",
        error.failure.stage, error.failure.code
    )?;
    if let Some(target) = error.target {
        write!(output, ",\"target\":\"{target}\"")?;
    }
    if let Some(detail) = error.detail {
        write!(output, ",\"detail\":\"{}\"", json_text(&detail))?;
    }
    writeln!(output, "}}")
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
    let mut scratch = [0; 65536];
    match options.selection {
        Selection::Account(account) => {
            physical(&store, deadline)?;
            check_account(&store, account, clock.as_ref(), deadline, &mut scratch)
                .map(|report| Verification::Account(Box::new(report)))
        }
        Selection::All => check_database(&store, clock.as_ref(), deadline, &mut scratch)
            .map(Verification::Database),
    }
}

fn physical(store: &IndexStore<'_>, deadline: Deadline) -> Result<(), Failure> {
    store
        .validate_integrity(deadline)
        .map_err(|error| adapter("physical", error))
}

fn check_database(
    store: &IndexStore<'_>,
    clock: &dyn Clock,
    deadline: Deadline,
    scratch: &mut [u8; 65536],
) -> Result<DatabaseChecks, Failure> {
    physical(store, deadline)?;
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
        let checked = check_account(store, account, clock, deadline, scratch)?;
        report.add(checked)?;
    }
    report.checked_at_ms = clock
        .sample()
        .map_err(|error| adapter("clock", error))?
        .utc_ms;
    Ok(report)
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
    if first.as_deref() == Some(OsStr::new("serve")) {
        let path = if args.next().as_deref() == Some(OsStr::new("--smtp-only")) {
            config_arguments(args)
        } else {
            None
        };
        let result = path
            .ok_or_else(|| config_failure("arguments", "invalid-arguments"))
            .and_then(|path| serve::run(&path));
        return match result {
            Ok(()) => Ok(ExitCode::SUCCESS),
            Err(error) => {
                let code = error.failure.exit_code();
                configuration_failure(&mut io::stdout().lock(), "serve", error)?;
                Ok(code)
            }
        };
    }
    if first.as_deref() == Some(OsStr::new("restore")) {
        let Some(options) = copy_arguments(args) else {
            restore_failure(
                &mut io::stdout().lock(),
                Failure {
                    stage: "arguments",
                    code: "invalid-arguments",
                }
                .into(),
            )?;
            return Ok(ExitCode::from(2));
        };
        return match restore(options) {
            Ok(receipt) => {
                let checks = receipt.checks;
                writeln!(io::stdout().lock(), "{{\"schema\":1,\"command\":\"restore\",\"status\":\"ok\",\"scope\":\"database\",\"source_epoch\":\"{}\",\"epoch\":\"{}\",\"copied_bytes\":{},\"physical_integrity\":true,\"checked_at_ms\":{},\"accounts\":{},\"metadata_rows\":{},\"mailboxes\":{},\"submissions\":{},\"recipients\":{},\"blobs\":{},\"body_bytes\":{},\"service_ready\":false}}", receipt.copy.epoch, receipt.epoch, receipt.copy.bytes, checks.checked_at_ms, checks.accounts, checks.metadata_rows, checks.mailboxes, checks.submissions, checks.recipients, checks.blobs, checks.body_bytes)?;
                Ok(ExitCode::SUCCESS)
            }
            Err(error) => {
                let code = error.failure.exit_code();
                restore_failure(&mut io::stdout().lock(), error)?;
                Ok(code)
            }
        };
    }
    if first.as_deref() == Some(OsStr::new("backup")) {
        let Some(options) = copy_arguments(args) else {
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
    if first.as_deref() == Some(OsStr::new("config")) {
        let path = if args.next().as_deref() == Some(OsStr::new("check")) {
            config_arguments(args)
        } else {
            None
        };
        let Some(path) = path else {
            config_check_failure(
                &mut io::stdout().lock(),
                config_failure("arguments", "invalid-arguments"),
            )?;
            return Ok(ExitCode::from(2));
        };
        return match config_check(&path) {
            Ok(inputs) => {
                writeln!(io::stdout().lock(), "{{\"schema\":1,\"command\":\"config.check\",\"status\":\"ok\",\"inputs\":{inputs}}}")?;
                Ok(ExitCode::SUCCESS)
            }
            Err(error) => {
                let code = error.failure.exit_code();
                config_check_failure(&mut io::stdout().lock(), error)?;
                Ok(code)
            }
        };
    }
    if first.as_deref() == Some(OsStr::new("store")) {
        let subcommand = args.next();
        if subcommand.as_deref() == Some(OsStr::new("init")) {
            let Some(options) = arguments(args) else {
                init_failure(
                    &mut io::stdout().lock(),
                    Failure {
                        stage: "arguments",
                        code: "invalid-arguments",
                    }
                    .into(),
                )?;
                return Ok(ExitCode::from(2));
            };
            return match init(options) {
                Ok(receipt) => {
                    writeln!(io::stdout().lock(), "{{\"schema\":1,\"command\":\"store.init\",\"status\":\"ok\",\"account\":\"{}\",\"epoch\":\"{}\",\"inbox\":\"{}\",\"sequence\":1,\"service_ready\":false}}", receipt.account, receipt.epoch, receipt.inbox)?;
                    Ok(ExitCode::SUCCESS)
                }
                Err(error) => {
                    let code = error.failure.exit_code();
                    init_failure(&mut io::stdout().lock(), error)?;
                    Ok(code)
                }
            };
        }
        if subcommand.as_deref() != Some(OsStr::new("verify")) {
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
