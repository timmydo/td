//! PostgreSQL connection qualification and bounded worker-owned I/O.
use crate::{
    clock::{RuntimeClock, TlsClockSource},
    ports::{Clock, Deadline},
};
use std::{
    net::{SocketAddr, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use td_crypto::{ClientConfig, ClockHandle, TlsProtocol, TlsSession, TrustStore};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Endpoint {
    Tcp(SocketAddr),
    Unix(td_fs::SocketPath),
}

/// The caller resolves and admits the address; TCP TLS authenticates server_name.
/// Borrowed credentials never enter diagnostics. DNS and bootstrap parsing are
/// separate cold operations, not hidden work on this connection worker.
pub struct Request<'a> {
    pub address: Endpoint,
    pub server_name: &'a str,
    pub user: &'a str,
    pub database: &'a str,
    pub password: &'a [u8],
    pub mode: Mode,
}

/// Preparation commands may use the owner; serving connections may not.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Preparation,
    Runtime,
    /// Restricted local mail reader; never ordinary primary or publisher access.
    ReplicaRead,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid,
    Deadline,
    Transport,
    Tls,
    Database,
    Settings,
    RuntimeSchema,
    RuntimeRole,
    RuntimePrivileges,
    RuntimeMaintenance,
    RuntimeReplica,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Status {
    pub server_version: u32,
}

const QUERY: &str = "SELECT pg_catalog.current_setting('server_version_num'), \
    pg_catalog.pg_is_in_recovery()::pg_catalog.text, \
    pg_catalog.current_setting('transaction_read_only'), \
    pg_catalog.current_setting('fsync'), pg_catalog.current_setting('full_page_writes')";
pub(crate) const MAX_DURATION: Duration = Duration::from_secs(10);
pub(crate) const MAX_BODY_DURATION: Duration = Duration::from_secs(30);

/// The longest deadline one owner Email import of `length` body bytes admits:
/// the ordinary ten seconds, plus up to twenty more in proportion to the body's
/// share of the stored-body ceiling, so a 64 MiB body has the stored-body
/// readers' thirty. Connection setup keeps its ten-second cap.
pub fn import_duration(length: u64) -> Duration {
    let ceiling = crate::postgres_body::MAX_BODY_BYTES;
    let extra = MAX_BODY_DURATION.saturating_sub(MAX_DURATION).as_nanos();
    let scaled = extra.saturating_mul(u128::from(length.min(ceiling))) / u128::from(ceiling);
    MAX_DURATION.saturating_add(Duration::from_nanos(
        u64::try_from(scaled).unwrap_or(u64::MAX),
    ))
}
/// Maximum caller-selected duration for historical owner audits only.
pub const MAX_AUDIT_DURATION: Duration = Duration::from_secs(3600);
#[cfg(test)]
const BODY_CHECK_TURNS: usize = 8;

/// Connect, authenticate, inspect primary/durability settings, and close.
/// One bounded worker owns the connection; no background thread or task.
/// This proves neither schema compatibility nor replication durability.
pub fn probe(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
) -> Result<Status, Error> {
    let status = with_client(request, trust, deadline, qualify)??;
    remaining(deadline)?;
    Ok(status)
}

// Only closed, bounded operations in this crate may use the client. A known
// mutation outcome survives a deadline crossed after its last call returned.
pub(crate) fn with_client<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    operation: impl FnOnce(&Client) -> T,
) -> Result<T, Error> {
    with_output_limit(request, trust, deadline, 64 * 1024, Input::new(), operation)
}

// Capacity observation: schema ledger plus at most 16 accounts, each with
// twenty-four bounded count queries, and global counters. No body contents.
pub(crate) fn with_capacity_client<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    operation: impl FnOnce(&Client) -> T,
) -> Result<T, Error> {
    with_output_limit(
        request,
        trust,
        deadline,
        256 * 1024,
        Input::capacity(),
        operation,
    )
}

// One bounded configuration-material operation, at most 128 KiB in two chunks.
pub(crate) fn with_material_client<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    operation: impl FnOnce(&Client) -> T,
) -> Result<T, Error> {
    with_output_limit(
        request,
        trust,
        deadline,
        256 * 1024,
        Input::new(),
        operation,
    )
}

// A complete mailbox listing has at most 257 result rows, including overflow.
pub(crate) fn with_mailbox_client<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    operation: impl FnOnce(&Client) -> T,
) -> Result<T, Error> {
    with_output_limit(
        request,
        trust,
        deadline,
        64 * 1024,
        Input::mailboxes(),
        operation,
    )
}

// At most 256 Email rows and 257 distinct mailbox rows, with protocol overhead.
pub(crate) fn with_mutation_client<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    operation: impl FnOnce(&Client) -> T,
) -> Result<T, Error> {
    with_output_limit(
        request,
        trust,
        deadline,
        128 * 1024,
        Input::mutations(),
        operation,
    )
}

// Whole-submission cancellation: 1000 recipients and schema/protocol overhead.
pub(crate) fn with_cancellation_client<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    operation: impl FnOnce(&Client) -> T,
) -> Result<T, Error> {
    with_output_limit(
        request,
        trust,
        deadline,
        128 * 1024,
        Input::cancellation(),
        operation,
    )
}

// Folder names or 100 maximum-length RCPT replies, plus protocol overhead.
pub(crate) fn with_batch_client<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    operation: impl FnOnce(&Client) -> T,
) -> Result<T, Error> {
    with_output_limit(
        request,
        trust,
        deadline,
        1024 * 1024,
        Input::mutations(),
        operation,
    )
}

// One immutable upload: at most 512 chunks in batches of eight.
pub(crate) fn with_upload_client<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    operation: impl FnOnce(&Client) -> T,
) -> Result<T, Error> {
    with_output_limit(
        request,
        trust,
        deadline,
        40 * 1024 * 1024,
        Input::mutations(),
        operation,
    )
}

// Owner import adds bounded source-mapping rows to a full-size body write.
pub(crate) fn with_import_client<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    body_length: u64,
    guarded: bool,
    operation: impl FnOnce(&Client) -> T,
) -> Result<T, Error> {
    let mut worker = connect_worker_bounded(
        request,
        trust,
        deadline,
        72 * 1024 * 1024,
        Input::imports(guarded),
        import_duration(body_length),
    )?;
    worker.run(operation)
}

// New-only mailbox creation also checks bounded parent/ancestor metadata.
pub(crate) fn with_import_mailbox_client<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    guarded: bool,
    operation: impl FnOnce(&Client) -> T,
) -> Result<T, Error> {
    with_output_limit(
        request,
        trust,
        deadline,
        if guarded { 2 * 1024 * 1024 } else { 128 * 1024 },
        if guarded {
            Input::reconciliation()
        } else {
            Input::mutations()
        },
        operation,
    )
}

// Reconciliation reads complete bounded source and local folder metadata.
pub(crate) fn with_reconcile_client<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    operation: impl FnOnce(&Client) -> T,
) -> Result<T, Error> {
    with_output_limit(
        request,
        trust,
        deadline,
        2 * 1024 * 1024,
        Input::reconciliation(),
        operation,
    )
}

// One SMTP transaction publishes a separate blob and receipt per local account.
pub(crate) fn with_fanout_client<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    accounts: usize,
    operation: impl FnOnce(&Client) -> T,
) -> Result<T, Error> {
    if !(1..=crate::config::identities::MAX_ACCOUNTS).contains(&accounts) {
        return Err(Error::Invalid);
    }
    with_output_limit(
        request,
        trust,
        deadline,
        40 * 1024 * 1024 * accounts,
        Input::fanout(accounts),
        operation,
    )
}

// At most 1000 retained recipient replies and one local notice, no remote body.
pub(crate) fn with_notice_client<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    operation: impl FnOnce(&Client) -> T,
) -> Result<T, Error> {
    with_output_limit(
        request,
        trust,
        deadline,
        20 * 1024 * 1024,
        Input::notices(),
        operation,
    )
}

// Explicit owner DDL needs more command completions than a probe.
pub(crate) fn with_schema_client<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    operation: impl FnOnce(&Client) -> T,
) -> Result<T, Error> {
    with_migration_client(request, trust, deadline, MAX_DURATION, operation)
}

pub(crate) fn with_migration_client<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    maximum: Duration,
    operation: impl FnOnce(&Client) -> T,
) -> Result<T, Error> {
    let mut worker = connect_worker_bounded(
        request,
        trust,
        deadline,
        128 * 1024,
        Input::schema(),
        maximum,
    )?;
    worker.run(operation)
}

// One complete source candidate, bounded independently of material/probe work.
pub(crate) fn with_configuration_client<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    operation: impl FnOnce(&Client) -> T,
) -> Result<T, Error> {
    with_output_limit(
        request,
        trust,
        deadline,
        4 * 1024 * 1024,
        Input::configuration(),
        operation,
    )
}

fn with_output_limit<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    output_limit: usize,
    input: Input,
    operation: impl FnOnce(&Client) -> T,
) -> Result<T, Error> {
    let mut worker = connect_worker(request, trust, deadline, output_limit, input)?;
    worker.run(operation)
}

// A single snapshot may need several synchronous reads through BlobReader.
// Budgets and deadlines belong to the connection and are never replenished.
pub(crate) fn with_body_worker<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    operation: impl FnOnce(&mut Worker) -> T,
) -> Result<T, Error> {
    let mut worker = body_worker(request, trust, deadline)?;
    Ok(operation(&mut worker))
}

pub(crate) fn with_audit_worker<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    budget: AuditBudget,
    operation: impl FnOnce(&mut Worker) -> T,
) -> Result<T, Error> {
    if request.mode != Mode::Preparation {
        return Err(Error::Invalid);
    }
    let mut worker = connect_authenticated_bounded(
        request,
        trust,
        deadline,
        budget.output,
        Input::audit(budget),
        MAX_AUDIT_DURATION,
    )?;
    worker.audit_windows = true;
    Ok(operation(&mut worker))
}

pub(crate) fn with_search_backfill_worker<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    operation: impl FnOnce(&mut Worker) -> T,
) -> Result<T, Error> {
    if request.mode != Mode::Preparation {
        return Err(Error::Invalid);
    }
    let mut worker = connect_worker_bounded(
        request,
        trust,
        deadline,
        24 * 1024 * 1024,
        Input::body(),
        MAX_BODY_DURATION,
    )?;
    Ok(operation(&mut worker))
}

pub(crate) fn body_worker(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
) -> Result<Worker, Error> {
    connect_worker_bounded(
        request,
        trust,
        deadline,
        512 * 1024,
        Input::body(),
        MAX_BODY_DURATION,
    )
}

// Only retained validators use this worker: every exposed turn qualifies schema
// and Runtime access under its own transaction-scoped migration lock.
pub(crate) fn validation_worker(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    budget: Option<ValidationBudget>,
) -> Result<Worker, Error> {
    if request.mode == Mode::ReplicaRead {
        return Err(Error::Invalid);
    }
    let (output, input) = match budget {
        Some(budget) => (budget.output(), Input::validation_checks(budget)),
        None => (64 * 1024, Input::mutations()),
    };
    connect_authenticated(request, trust, deadline, output, input)
}

fn connect_worker(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    output_limit: usize,
    input: Input,
) -> Result<Worker, Error> {
    connect_worker_bounded(request, trust, deadline, output_limit, input, MAX_DURATION)
}

fn connect_worker_bounded(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    output_limit: usize,
    input: Input,
    maximum: Duration,
) -> Result<Worker, Error> {
    let mut worker =
        connect_authenticated_bounded(request, trust, deadline, output_limit, input, maximum)?;
    let mode = request.mode;
    if mode != Mode::Preparation {
        let schema = worker.run(|client| {
            let receipt = crate::postgres_schema::lock_and_inspect(client)
                .map_err(|_| Error::RuntimeSchema)?;
            qualify_runtime_mode(client, mode, Some(receipt.store_epoch))?;
            Ok(receipt)
        })??;
        worker
            .client
            .as_mut()
            .ok_or(Error::Database)?
            .runtime_schema = Some(schema);
    }
    Ok(worker)
}

pub(crate) fn qualify_runtime(client: &Client) -> Result<(), Error> {
    qualify_runtime_mode(client, Mode::Runtime, None)
}

pub(crate) fn qualify_runtime_mode(
    client: &Client,
    mode: Mode,
    epoch: Option<crate::ids::StoreEpoch>,
) -> Result<(), Error> {
    let mut timing = crate::performance::Timer::start(crate::performance::Phase::DbRuntime);
    let result = match mode {
        Mode::Runtime => crate::postgres_runtime_access::qualify(client),
        Mode::ReplicaRead => crate::postgres_runtime_access::qualify_replica_reader(
            client,
            epoch.ok_or(Error::Invalid)?,
        ),
        Mode::Preparation => return Err(Error::Invalid),
    }
    .map_err(|error| match error {
        crate::postgres_runtime_access::Error::Role => Error::RuntimeRole,
        crate::postgres_runtime_access::Error::Maintenance => Error::RuntimeMaintenance,
        crate::postgres_runtime_access::Error::Replica => Error::RuntimeReplica,
        _ => Error::RuntimePrivileges,
    });
    if result.is_ok() {
        timing.succeeded();
    }
    result
}

fn connect_authenticated(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    output_limit: usize,
    input: Input,
) -> Result<Worker, Error> {
    connect_authenticated_bounded(request, trust, deadline, output_limit, input, MAX_DURATION)
}

fn connect_authenticated_bounded(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    output_limit: usize,
    input: Input,
    maximum: Duration,
) -> Result<Worker, Error> {
    connect_authenticated_until(
        request,
        trust,
        deadline,
        deadline,
        output_limit,
        input,
        maximum,
    )
}

/// The session settings every connection starts with.
const OPTIONS: &str = "-c statement_timeout=5000 -c lock_timeout=1000 -c idle_in_transaction_session_timeout=10000 -c search_path=pg_catalog";
/// The largest backend message, the frame filter's 128 KiB less the
/// length word td-postgres does not count.
const MESSAGE_BYTES: usize = 128 * 1024 - 4;
/// The largest request td-postgres accepts at all.
const REQUEST_BYTES: usize = 1 << 30;
/// Notifications held between two reads of them; they are hints.
const NOTIFICATIONS: usize = 64;
/// The PBKDF2 work a server may ask of a login.
const SCRAM_ITERATIONS: u32 = 100_000;

/// td-postgres's per-operation bounds. The input filter's connection
/// totals stay the binding ones, so each limit here is no tighter.
fn limits(output_limit: usize, input: &Input) -> td_postgres::Limits {
    td_postgres::Limits {
        message_bytes: MESSAGE_BYTES,
        request_bytes: output_limit.clamp(64, REQUEST_BYTES),
        messages: input.frames().max(1),
        rows: input.frames(),
        columns: 1664,
        // Each value costs at least four wire bytes and eight here.
        result_bytes: input.bytes().saturating_mul(2).min(u32::MAX as usize),
        notifications: NOTIFICATIONS,
        scram_iterations: SCRAM_ITERATIONS,
    }
}

fn connect_authenticated_until(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    connect_deadline: Instant,
    output_limit: usize,
    input: Input,
    maximum: Duration,
) -> Result<Worker, Error> {
    let mut timing = crate::performance::Timer::start(crate::performance::Phase::DbConnect);
    let allowance = remaining(deadline)?;
    let connect_deadline = connect_deadline
        .min(deadline)
        .min(Instant::now() + MAX_DURATION);
    if allowance > maximum
        || (matches!(request.address, Endpoint::Tcp(_)) && request.server_name.is_empty())
        || request.server_name.len() > 253
        || request.user.is_empty()
        || request.user.len() > 256
        || request.database.is_empty()
        || request.database.len() > 256
        || request.user.contains('\0')
        || request.database.contains('\0')
        || request.password.is_empty()
        || request.password.len() > 4096
        || !request.password.is_ascii()
        || request.password.contains(&0)
    {
        return Err(Error::Invalid);
    }
    let input = if request.mode != Mode::Preparation {
        input.runtime()
    } else {
        input
    };
    let validation_window = input.window();
    let limits = limits(output_limit, &input);
    let config = td_postgres::Config {
        user: request.user,
        database: request.database,
        password: request.password,
        application_name: "td-mta-postgres",
        parameters: &[("options", OPTIONS)],
    };
    let abort = Arc::new(AtomicBool::new(false));
    // A dial that fails before any byte moves is a transport refusal,
    // which lets the caller try another address.
    let dialed = AtomicBool::new(false);
    let session = match request.address {
        Endpoint::Tcp(address) => {
            let connector = tls_connector(
                trust,
                request.server_name,
                deadline,
                connect_deadline,
                output_limit,
                input,
                Arc::clone(&abort),
            )?;
            // Transitional, with the TCP endpoint: td-mta connects, so a
            // failed connect stays a transport refusal.
            let mut tcp = crate::performance::Timer::start(crate::performance::Phase::DbTcp);
            let socket = TcpStream::connect_timeout(&address, remaining(connect_deadline)?)
                .map_err(|_| Error::Transport)?;
            tcp.succeeded();
            dialed.store(true, Ordering::Relaxed);
            let mut handshake =
                crate::performance::Timer::start(crate::performance::Phase::DbHandshake);
            let session =
                td_postgres::start_tls(socket, connector, &config, limits, connect_deadline);
            if session.is_ok() {
                handshake.succeeded();
            }
            session
        }
        Endpoint::Unix(path) => {
            // Socket admission precedes and stays apart from the dial.
            td_fs::check_server_socket(path.as_path()).map_err(|_| Error::Transport)?;
            let mut handshake =
                crate::performance::Timer::start(crate::performance::Phase::DbHandshake);
            let session = td_postgres::connect_unix_through(
                path.as_path(),
                |socket| {
                    dialed.store(true, Ordering::Relaxed);
                    Wire::plain(socket, input, output_limit, Arc::clone(&abort))
                },
                &config,
                limits,
                connect_deadline,
            );
            if session.is_ok() {
                handshake.succeeded();
            }
            session
        }
    }
    .map_err(|error| match error {
        DbError::Deadline => Error::Deadline,
        DbError::Io | DbError::Closed if !dialed.load(Ordering::Relaxed) => Error::Transport,
        DbError::Invalid => Error::Invalid,
        _ => Error::Database,
    })?;
    timing.succeeded();
    Ok(Worker {
        client: Some(Box::new(Client::new(session, deadline))),
        deadline,
        failed: None,
        abort,
        validation_window,
        audit_windows: false,
        relay_wake: false,
    })
}

/// td-crypto's verified session for `server_name`, as td-postgres's TLS
/// connector. Transitional, with the TCP endpoint.
fn tls_connector(
    trust: &TrustStore,
    server_name: &str,
    deadline: Instant,
    connect_deadline: Instant,
    output_limit: usize,
    input: Input,
    abort: Arc<AtomicBool>,
) -> Result<wire::Connector, Error> {
    let clock: Arc<dyn Clock> = Arc::new(RuntimeClock::new());
    let now = clock.sample().map_err(|_| Error::Deadline)?.monotonic;
    let cap = Deadline::after(now, transport_milliseconds(remaining(deadline)?)?)
        .map_err(|_| Error::Invalid)?;
    let handshake_cap = Deadline::after(now, transport_milliseconds(remaining(connect_deadline)?)?)
        .map_err(|_| Error::Invalid)?
        .min(cap);
    let tls_clock = Arc::new(ClockHandle::new(TlsClockSource::new(clock.clone())));
    let policy = Arc::new(
        ClientConfig::new(trust, tls_clock, TlsProtocol::Postgres).map_err(|_| Error::Tls)?,
    );
    let session = TlsSession::client(policy, server_name).map_err(|_| Error::Tls)?;
    Ok(wire::Connector {
        session,
        clock,
        cap,
        handshake_cap,
        output: output_limit,
        input,
        abort,
    })
}

pub(crate) struct Worker {
    // Boxed: a worker sits inline in per-attempt state of bounded size.
    client: Option<Box<Client>>,
    deadline: Instant,
    failed: Option<Error>,
    /// Shared with the stream: set, nothing more is written or read.
    abort: Arc<AtomicBool>,
    validation_window: Option<windows::Windows>,
    audit_windows: bool,
    relay_wake: bool,
}

impl Worker {
    pub(crate) fn begin_validation_turn(&mut self) -> Result<(), Error> {
        self.validation_window
            .as_ref()
            .map_or(Ok(()), |window| window.begin())
            .map_err(|_| Error::Database)
    }
    pub(crate) fn finish_validation_turn(&mut self) -> Result<(), Error> {
        self.validation_window
            .as_ref()
            .map_or(Ok(()), |window| window.finish())
            .map_err(|_| Error::Database)
    }
    pub(crate) fn constrain_deadline(&mut self, deadline: Instant) -> Result<(), Error> {
        self.deadline = self.deadline.min(deadline);
        self.check()
    }
    fn fail(&mut self, error: Error) -> Error {
        let error = *self.failed.get_or_insert(error);
        self.abort.store(true, Ordering::Release);
        error
    }
    pub(crate) fn check_deadline(&mut self) -> Result<(), Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if let Err(error) = remaining(self.deadline) {
            return Err(self.fail(error));
        }
        Ok(())
    }
    // No query or round trip: read what the idle connection already holds,
    // so a cached session cannot hide a server-ended connection behind a
    // notice.
    pub(crate) fn check(&mut self) -> Result<(), Error> {
        self.check_deadline()?;
        let client = self.client.as_ref().ok_or(Error::Database)?;
        let drained = client.drain(Instant::now());
        self.relay_wake |= client.take_relay();
        match (drained, client.fault()) {
            (_, Some(error)) => Err(self.fail(error)),
            (Err(_), None) => Err(self.fail(Error::Database)),
            (Ok(()), None) => Ok(()),
        }
    }
    /// Wait until `until` for a notification on the idle connection;
    /// true once the relay's hint has arrived.
    pub(crate) fn wait_relay(&mut self, until: Instant) -> Result<bool, Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if Instant::now() >= until.min(self.deadline) {
            return Ok(std::mem::take(&mut self.relay_wake));
        }
        let client = self.client.as_ref().ok_or(Error::Database)?;
        let waited = client.wait(until.min(self.deadline));
        self.relay_wake |= client.take_relay();
        match (waited, client.fault()) {
            (_, Some(error)) => Err(self.fail(error)),
            (Err(_), None) => Err(self.fail(Error::Database)),
            (Ok(()), None) => Ok(std::mem::take(&mut self.relay_wake)),
        }
    }

    /// Run one operation's SQL under the worker's deadline. A failure that
    /// ends the connection or crosses the deadline is the worker's error,
    /// whatever the operation made of it; a statement's own error is the
    /// operation's result.
    pub(crate) fn run<T>(&mut self, operation: impl FnOnce(&Client) -> T) -> Result<T, Error> {
        let mut timing = crate::performance::Timer::start(crate::performance::Phase::DbWork);
        self.check_deadline()?;
        let audit_window = self
            .validation_window
            .as_ref()
            .filter(|_| self.audit_windows);
        if let Some(window) = audit_window {
            window.begin().map_err(|_| Error::Database)?;
        }
        let client = self.client.as_ref().ok_or(Error::Database)?;
        client.set_deadline(self.deadline);
        let result = operation(client);
        self.relay_wake |= client.take_relay();
        if let Some(error) = client.fault() {
            return Err(self.fail(error));
        }
        if let Some(window) = audit_window {
            window.finish().map_err(|_| Error::Database)?;
        }
        timing.succeeded();
        Ok(result)
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let mut timing = crate::performance::Timer::start(crate::performance::Phase::DbClose);
        // Cleanup must neither renew a job nor alter its outcome: a failed
        // or expired connection is abandoned, a healthy one gets Terminate
        // within the original deadline and at most 100 ms.
        let Some(client) = self.client.take() else {
            return;
        };
        if self.failed.is_some() || client.fault().is_some() || Instant::now() >= self.deadline {
            self.abort.store(true, Ordering::Release);
            return;
        }
        let until = self
            .deadline
            .min(Instant::now() + Duration::from_millis(100));
        if client.close(until) {
            timing.succeeded();
        }
    }
}

/// The settings check: one row of five text values.
pub(crate) fn qualify(client: &Client) -> Result<Status, Error> {
    let rows = client.query(QUERY, &[]).map_err(|_| Error::Database)?;
    let row = match (rows.len(), rows.first()) {
        (1, Some(row)) if row.len() == 5 => row,
        _ => return Err(Error::Settings),
    };
    let value = |index: usize| row.try_get::<_, &str>(index).map_err(|_| Error::Settings);
    let server_version = value(0)?.parse().map_err(|_| Error::Settings)?;
    if value(1)? != "false" || value(2)? != "off" || value(3)? != "on" || value(4)? != "on" {
        return Err(Error::Settings);
    }
    Ok(Status { server_version })
}

// TLS uses whole ticks; the worker's exact Instant remains authoritative.
// Rounding down can turn sub-millisecond expiry into a generic TLS failure
// before the worker sees its own deadline.
fn transport_milliseconds(allowance: Duration) -> Result<u64, Error> {
    u64::try_from(allowance.as_nanos().div_ceil(1_000_000)).map_err(|_| Error::Invalid)
}

fn remaining(deadline: Instant) -> Result<Duration, Error> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or(Error::Deadline)
}

mod client;
pub(crate) use client::{
    Client, DbError, Encode, FromColumn, OneRow, Row, RowIter, Rows, SqlState,
};

mod wire;
use wire::Wire;

mod audit_budget;
pub(crate) use audit_budget::AuditBudget;
mod retained;
pub(crate) use retained::{ReadIsolation, ReadSession};
mod replica;
pub(crate) use replica::ReplicaSession;
mod mutation;
pub(crate) use mutation::MutationSession;
mod authentication;
pub(crate) use authentication::AuthenticationSession;
mod validation_budget;
pub(crate) use validation_budget::ValidationBudget;
mod framing;
mod windows;
use framing::Input;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::{io, net::TcpListener, thread};

    #[test]
    fn transport_tick_never_expires_before_exact_worker_allowance() {
        for nanos in [
            1,
            999_999,
            1_000_000,
            1_000_001,
            499_999_999,
            500_000_000,
            9_999_999_999,
        ] {
            let exact = Duration::from_nanos(nanos);
            let rounded = Duration::from_millis(transport_milliseconds(exact).unwrap());
            assert!(rounded >= exact);
            assert!(rounded - exact < Duration::from_millis(1));
        }
        assert_eq!(transport_milliseconds(Duration::MAX), Err(Error::Invalid));
    }
    #[test]
    fn email_import_allowance_scales_with_body_to_the_body_ceiling() {
        let ceiling = crate::postgres_body::MAX_BODY_BYTES;
        assert_eq!(import_duration(0), MAX_DURATION);
        assert_eq!(import_duration(ceiling / 2), Duration::from_secs(20));
        assert_eq!(import_duration(ceiling), MAX_BODY_DURATION);
        assert_eq!(import_duration(ceiling + 1), MAX_BODY_DURATION);
        assert_eq!(import_duration(u64::MAX), MAX_BODY_DURATION);
        let mut previous = MAX_DURATION;
        for length in (0..=ceiling).step_by(1 << 20) {
            let allowance = import_duration(length);
            assert!(allowance >= previous && allowance <= MAX_BODY_DURATION);
            previous = allowance;
        }
    }

    #[test]
    fn replica_reader_cannot_open_outbound_validation_worker() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let request = Request {
            address: Endpoint::Tcp(listener.local_addr().unwrap()),
            server_name: "localhost",
            mode: Mode::ReplicaRead,
            user: "reader",
            database: "probe",
            password: b"fixture",
        };
        let trust = TrustStore::public_roots().unwrap();
        assert!(matches!(
            validation_worker(&request, &trust, Instant::now() + MAX_DURATION, None),
            Err(Error::Invalid)
        ));
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
    }

    #[test]
    fn stalled_ssl_upgrade_obeys_original_worker_deadline() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let peer = thread::spawn(move || {
            let stop = Instant::now() + Duration::from_secs(2);
            while Instant::now() < stop {
                match listener.accept() {
                    Ok((socket, _)) => {
                        thread::sleep(Duration::from_millis(200));
                        drop(socket);
                        return;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("accept failed: {error}"),
                }
            }
            // A scheduler delay can exhaust the deadline before dialing.
        });
        let request = Request {
            address: Endpoint::Tcp(address),
            server_name: "localhost",
            mode: Mode::Preparation,
            user: "probe",
            database: "probe",
            password: b"fixture",
        };
        let trust = TrustStore::public_roots().unwrap();
        assert_eq!(
            probe(&request, &trust, Instant::now() + Duration::from_millis(50)),
            Err(Error::Deadline)
        );
        peer.join().unwrap();
    }

    /// td-postgres does no SASLprep: a password it cannot log in with is
    /// refused before anything is dialed.
    #[test]
    fn a_non_ascii_or_nul_password_is_refused_before_dialing() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let trust = TrustStore::public_roots().unwrap();
        for password in [&b"p\xc3\xa9"[..], b"\x80", b"pass\0word"] {
            let request = Request {
                address: Endpoint::Tcp(listener.local_addr().unwrap()),
                server_name: "localhost",
                mode: Mode::Preparation,
                user: "probe",
                database: "probe",
                password,
            };
            assert_eq!(
                probe(&request, &trust, Instant::now() + MAX_DURATION),
                Err(Error::Invalid)
            );
        }
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
    }
}
