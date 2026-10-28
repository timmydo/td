//! Inert worker-side PostgreSQL connection qualification; no serving activation.
use crate::{
    clock::{RuntimeClock, TlsClockSource},
    ports::{Clock, Deadline, FlushProgress, IoProgress, Transport},
    tls_io::{TlsIo, TLS_WIRE_BYTES},
    transport::TcpTransport,
};
use std::{
    future::{poll_fn, Future},
    io,
    net::{SocketAddr, TcpStream},
    pin::{pin, Pin},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll, Wake, Waker},
    thread,
    time::{Duration, Instant},
};
use td_crypto::{ClientConfig, ClockHandle, TlsProtocol, TlsSession, TrustStore};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_postgres::{
    config::{ChannelBinding, SslMode, SslNegotiation},
    tls, Config, SimpleQueryMessage,
};

/// The caller resolves and admits the address; TLS authenticates server_name.
/// Borrowed credentials never enter diagnostics. DNS and bootstrap parsing are
/// separate cold operations, not hidden work on this connection worker.
pub struct Request<'a> {
    pub address: SocketAddr,
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
/// One bounded worker owns every future; no Tokio runtime or background task.
/// This proves neither schema compatibility nor replication durability.
pub fn probe(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
) -> Result<Status, Error> {
    let status = with_client(request, trust, deadline, |client| Box::pin(qualify(client)))??;
    remaining(deadline)?;
    Ok(status)
}

// Only closed, bounded operations in this crate may use the driver. A known
// mutation outcome must survive a deadline crossed while completing its poll.
pub(crate) fn with_client<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    operation: impl for<'a> FnOnce(&'a tokio_postgres::Client) -> Pin<Box<dyn Future<Output = T> + 'a>>,
) -> Result<T, Error> {
    with_output_limit(request, trust, deadline, 64 * 1024, Input::new(), operation)
}

// One bounded configuration-material operation, at most 128 KiB in two chunks.
pub(crate) fn with_material_client<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    operation: impl for<'a> FnOnce(&'a tokio_postgres::Client) -> Pin<Box<dyn Future<Output = T> + 'a>>,
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
    operation: impl for<'a> FnOnce(&'a tokio_postgres::Client) -> Pin<Box<dyn Future<Output = T> + 'a>>,
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

/// One fresh operation's client and continuously held migration qualification.
/// Construction stays here; neither the client nor its receipt can be replaced.
pub(crate) struct ScopedClient<'a> {
    client: &'a tokio_postgres::Client,
    runtime_schema: Option<crate::postgres_schema::Receipt>,
}
impl<'a> ScopedClient<'a> {
    pub(crate) fn client(&self) -> &'a tokio_postgres::Client {
        self.client
    }
    pub(crate) async fn inspect_schema(
        &self,
    ) -> Result<crate::postgres_schema::Receipt, crate::postgres_schema::Error> {
        match self.runtime_schema {
            Some(receipt) => Ok(receipt),
            None => crate::postgres_schema::lock_and_inspect(self.client).await,
        }
    }
}

pub(crate) fn with_scoped_mailbox_client<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    operation: impl for<'a> FnOnce(ScopedClient<'a>) -> Pin<Box<dyn Future<Output = T> + 'a>>,
) -> Result<T, Error> {
    with_scoped_output_limit(
        request,
        trust,
        deadline,
        64 * 1024,
        Input::mailboxes(),
        operation,
    )
}

pub(crate) fn with_scoped_mutation_client<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    operation: impl for<'a> FnOnce(ScopedClient<'a>) -> Pin<Box<dyn Future<Output = T> + 'a>>,
) -> Result<T, Error> {
    with_scoped_output_limit(
        request,
        trust,
        deadline,
        128 * 1024,
        Input::mutations(),
        operation,
    )
}

fn with_scoped_output_limit<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    output_limit: usize,
    input: Input,
    operation: impl for<'a> FnOnce(ScopedClient<'a>) -> Pin<Box<dyn Future<Output = T> + 'a>>,
) -> Result<T, Error> {
    let (mut worker, runtime_schema) =
        connect_worker_with_schema(request, trust, deadline, output_limit, input, MAX_DURATION)?;
    worker.run(move |client| {
        operation(ScopedClient {
            client,
            runtime_schema,
        })
    })
}

// At most 256 Email rows and 257 distinct mailbox rows, with protocol overhead.
pub(crate) fn with_mutation_client<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    operation: impl for<'a> FnOnce(&'a tokio_postgres::Client) -> Pin<Box<dyn Future<Output = T> + 'a>>,
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
    operation: impl for<'a> FnOnce(&'a tokio_postgres::Client) -> Pin<Box<dyn Future<Output = T> + 'a>>,
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
    operation: impl for<'a> FnOnce(&'a tokio_postgres::Client) -> Pin<Box<dyn Future<Output = T> + 'a>>,
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
    operation: impl for<'a> FnOnce(&'a tokio_postgres::Client) -> Pin<Box<dyn Future<Output = T> + 'a>>,
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
    operation: impl for<'a> FnOnce(&'a tokio_postgres::Client) -> Pin<Box<dyn Future<Output = T> + 'a>>,
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
    operation: impl for<'a> FnOnce(&'a tokio_postgres::Client) -> Pin<Box<dyn Future<Output = T> + 'a>>,
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
    operation: impl for<'a> FnOnce(&'a tokio_postgres::Client) -> Pin<Box<dyn Future<Output = T> + 'a>>,
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
    operation: impl for<'a> FnOnce(&'a tokio_postgres::Client) -> Pin<Box<dyn Future<Output = T> + 'a>>,
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
    operation: impl for<'a> FnOnce(&'a tokio_postgres::Client) -> Pin<Box<dyn Future<Output = T> + 'a>>,
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
    operation: impl for<'a> FnOnce(&'a tokio_postgres::Client) -> Pin<Box<dyn Future<Output = T> + 'a>>,
) -> Result<T, Error> {
    with_output_limit(
        request,
        trust,
        deadline,
        // Prepared names grow with the driver's process-wide statement counter.
        128 * 1024,
        Input::schema(),
        operation,
    )
}

// One complete source candidate, bounded independently of material/probe work.
pub(crate) fn with_configuration_client<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    operation: impl for<'a> FnOnce(&'a tokio_postgres::Client) -> Pin<Box<dyn Future<Output = T> + 'a>>,
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
    operation: impl for<'a> FnOnce(&'a tokio_postgres::Client) -> Pin<Box<dyn Future<Output = T> + 'a>>,
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
        16 * 1024 * 1024,
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
    connect_worker_with_schema(request, trust, deadline, output_limit, input, maximum)
        .map(|(worker, _)| worker)
}

fn connect_worker_with_schema(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    output_limit: usize,
    input: Input,
    maximum: Duration,
) -> Result<(Worker, Option<crate::postgres_schema::Receipt>), Error> {
    let mut worker =
        connect_authenticated_bounded(request, trust, deadline, output_limit, input, maximum)?;
    let runtime_schema = if request.mode == Mode::Runtime {
        Some(worker.run(|client| {
            Box::pin(async move {
                let receipt = crate::postgres_schema::lock_and_inspect(client)
                    .await
                    .map_err(|_| Error::RuntimeSchema)?;
                qualify_runtime(client).await?;
                Ok(receipt)
            })
        })??)
    } else {
        None
    };
    Ok((worker, runtime_schema))
}

pub(crate) async fn qualify_runtime(client: &tokio_postgres::Client) -> Result<(), Error> {
    let mut timing = crate::performance::Timer::start(crate::performance::Phase::DbRuntime);
    let result = crate::postgres_runtime_access::qualify(client)
        .await
        .map_err(|error| match error {
            crate::postgres_runtime_access::Error::Role => Error::RuntimeRole,
            crate::postgres_runtime_access::Error::Maintenance => Error::RuntimeMaintenance,
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
        || request.server_name.is_empty()
        || request.server_name.len() > 253
        || request.user.is_empty()
        || request.user.len() > 256
        || request.database.is_empty()
        || request.database.len() > 256
        || request.user.contains('\0')
        || request.database.contains('\0')
        || request.password.is_empty()
        || request.password.len() > 4096
    {
        return Err(Error::Invalid);
    }
    let clock: Arc<dyn Clock> = Arc::new(RuntimeClock::new());
    let now = clock.sample().map_err(|_| Error::Deadline)?.monotonic;
    let milliseconds = transport_milliseconds(allowance)?;
    let cap = Deadline::after(now, milliseconds).map_err(|_| Error::Invalid)?;
    let handshake_cap = Deadline::after(now, transport_milliseconds(remaining(connect_deadline)?)?)
        .map_err(|_| Error::Invalid)?
        .min(cap);
    let tls_clock = Arc::new(ClockHandle::new(TlsClockSource::new(clock.clone())));
    let policy = Arc::new(
        ClientConfig::new(trust, tls_clock, TlsProtocol::Postgres).map_err(|_| Error::Tls)?,
    );
    let session = TlsSession::client(policy, request.server_name).map_err(|_| Error::Tls)?;
    let mut tcp_timing = crate::performance::Timer::start(crate::performance::Phase::DbTcp);
    let socket = TcpStream::connect_timeout(&request.address, remaining(connect_deadline)?)
        .map_err(|_| Error::Transport)?;
    tcp_timing.succeeded();
    drop(tcp_timing);
    let activity = Arc::new(AtomicBool::new(false));
    let socket = Arc::new(Mutex::new(DriverSocket {
        transport: TcpTransport::from_stream(socket).map_err(|_| Error::Transport)?,
        deadline: connect_deadline,
    }));
    let transport = Network {
        socket: socket.clone(),
        activity: activity.clone(),
    };
    let mut config = Config::new();
    config
        .host(request.server_name)
        .user(request.user)
        .dbname(request.database)
        .password(request.password)
        .application_name("td-mta-postgres")
        .options("-c statement_timeout=5000 -c lock_timeout=1000 -c search_path=pg_catalog")
        .ssl_mode(SslMode::Require)
        .ssl_negotiation(SslNegotiation::Postgres)
        .channel_binding(ChannelBinding::Disable);
    let input = if request.mode == Mode::Runtime {
        input.runtime()
    } else {
        input
    };
    let validation_window = input.window();
    let connector = Connector {
        session,
        clock,
        cap,
        handshake_cap,
        output_limit,
        input,
    };
    let waker = Waker::from(Arc::new(WorkerWake(thread::current(), activity.clone())));
    let mut context = Context::from_waker(&waker);
    let mut handshake_timing =
        crate::performance::Timer::start(crate::performance::Phase::DbHandshake);
    let mut connect = pin!(config.connect_raw(Io::new(transport, false), connector));
    let (client, connection) = loop {
        remaining(connect_deadline)?;
        activity.store(false, Ordering::Relaxed);
        if let Poll::Ready(result) = connect.as_mut().poll(&mut context) {
            break result.map_err(|_| Error::Database)?;
        }
        if !activity.load(Ordering::Relaxed) {
            wait_socket(&socket, connect_deadline)?;
        }
    };
    handshake_timing.succeeded();
    drop(handshake_timing);
    timing.succeeded();
    socket.lock().map_err(|_| Error::Transport)?.deadline = deadline;
    Ok(Worker {
        client: Some(client),
        connection: Box::pin(connection),
        deadline,
        waker,
        failed: None,
        activity,
        socket,
        validation_window,
        audit_windows: false,
        relay_wake: false,
    })
}

pub(crate) struct Worker {
    client: Option<tokio_postgres::Client>,
    connection: Pin<Box<tokio_postgres::Connection<Io<Network>, Io<Secure>>>>,
    deadline: Instant,
    waker: Waker,
    failed: Option<Error>,
    activity: Arc<AtomicBool>,
    socket: Arc<Mutex<DriverSocket>>,
    validation_window: Option<windows::Windows>,
    audit_windows: bool,
    relay_wake: bool,
}

fn relay_notification(message: &tokio_postgres::AsyncMessage) -> bool {
    matches!(message, tokio_postgres::AsyncMessage::Notification(value)
        if value.channel() == "td_mta_relay" && value.payload().is_empty())
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
        self.socket.lock().map_err(|_| Error::Transport)?.deadline = self.deadline;
        self.check()
    }
    pub(crate) fn check_deadline(&mut self) -> Result<(), Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if let Err(error) = remaining(self.deadline) {
            self.failed = Some(error);
            return Err(error);
        }
        Ok(())
    }
    // No query or round trip: drain available asynchronous messages so an idle
    // cached read cannot hide a terminal connection error behind a notice.
    pub(crate) fn check(&mut self) -> Result<(), Error> {
        self.socket.lock().map_err(|_| Error::Transport)?.deadline = self.deadline;
        if let Some(error) = self.failed {
            return Err(error);
        }
        let mut context = Context::from_waker(&self.waker);
        loop {
            if let Err(error) = remaining(self.deadline) {
                self.failed = Some(error);
                return Err(error);
            }
            self.activity.store(false, Ordering::Relaxed);
            match self.connection.as_mut().poll_message(&mut context) {
                Poll::Pending if !self.activity.load(Ordering::Relaxed) => return Ok(()),
                Poll::Pending => {}
                Poll::Ready(Some(Ok(message))) => {
                    self.relay_wake |= relay_notification(&message);
                }
                Poll::Ready(Some(Err(_))) | Poll::Ready(None) => {
                    self.failed = Some(Error::Database);
                    return Err(Error::Database);
                }
            }
        }
    }

    pub(crate) fn run<T>(
        &mut self,
        operation: impl for<'a> FnOnce(
            &'a tokio_postgres::Client,
        ) -> Pin<Box<dyn Future<Output = T> + 'a>>,
    ) -> Result<T, Error> {
        self.socket.lock().map_err(|_| Error::Transport)?.deadline = self.deadline;
        let mut timing = crate::performance::Timer::start(crate::performance::Phase::DbWork);
        if let Some(error) = self.failed {
            return Err(error);
        }
        let mut context = Context::from_waker(&self.waker);
        let audit_window = self
            .validation_window
            .as_ref()
            .filter(|_| self.audit_windows);
        if let Some(window) = audit_window {
            window.begin().map_err(|_| Error::Database)?;
        }
        let client = self.client.as_ref().ok_or(Error::Database)?;
        let mut operation = operation(client);
        loop {
            if let Err(error) = remaining(self.deadline) {
                self.failed = Some(error);
                return Err(error);
            }
            self.activity.store(false, Ordering::Relaxed);
            if let Poll::Ready(result) = operation.as_mut().poll(&mut context) {
                if let Some(window) = audit_window {
                    window.finish().map_err(|_| Error::Database)?;
                }
                timing.succeeded();
                return Ok(result);
            }
            match self.connection.as_mut().poll_message(&mut context) {
                Poll::Ready(Some(Ok(message))) => {
                    self.relay_wake |= relay_notification(&message);
                    continue;
                }
                Poll::Pending => {}
                Poll::Ready(Some(Err(_))) | Poll::Ready(None) => {
                    self.failed = Some(Error::Database);
                    return Err(Error::Database);
                }
            }
            if !self.activity.load(Ordering::Relaxed) {
                if let Err(error) = wait_socket(&self.socket, self.deadline) {
                    self.failed = Some(error);
                    return Err(error);
                }
            }
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let mut timing = crate::performance::Timer::start(crate::performance::Phase::DbClose);
        // Releasing the last sender lets the driver drain responses and send
        // Terminate. Cleanup must neither renew a job nor alter its outcome.
        drop(self.client.take());
        if self.failed.is_some() {
            return;
        }
        let until = self
            .deadline
            .min(Instant::now() + Duration::from_millis(100));
        if let Ok(mut socket) = self.socket.lock() {
            socket.deadline = until;
        } else {
            return;
        }
        let mut context = Context::from_waker(&self.waker);
        while Instant::now() < until {
            self.activity.store(false, Ordering::Relaxed);
            match self.connection.as_mut().poll_message(&mut context) {
                Poll::Ready(None) => {
                    timing.succeeded();
                    return;
                }
                Poll::Ready(Some(Err(_))) => return,
                Poll::Ready(Some(Ok(_))) => (),
                Poll::Pending if !self.activity.load(Ordering::Relaxed) => {
                    if wait_socket(&self.socket, until).is_err() {
                        return;
                    }
                }
                Poll::Pending => (),
            }
        }
    }
}

pub(crate) async fn qualify(client: &tokio_postgres::Client) -> Result<Status, Error> {
    let messages = client
        .simple_query(QUERY)
        .await
        .map_err(|_| Error::Database)?;
    let mut status = None;
    for message in messages {
        if let SimpleQueryMessage::Row(row) = message {
            if status.is_some() || row.len() != 5 {
                return Err(Error::Settings);
            }
            let value = |index| row.try_get(index).ok().flatten().ok_or(Error::Settings);
            let server_version = value(0)?.parse().map_err(|_| Error::Settings)?;
            if value(1)? != "false" || value(2)? != "off" || value(3)? != "on" || value(4)? != "on"
            {
                return Err(Error::Settings);
            }
            status = Some(Status { server_version });
        }
    }
    status.ok_or(Error::Settings)
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

struct WorkerWake(thread::Thread, Arc<AtomicBool>);
impl Wake for WorkerWake {
    fn wake(self: Arc<Self>) {
        self.1.store(true, Ordering::Relaxed);
        self.0.unpark();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.1.store(true, Ordering::Relaxed);
        self.0.unpark();
    }
}

// TLS can consume an encrypted header and report no plaintext yet. Track
// actual socket progress below it so the worker does not sleep on that work.
struct DriverSocket {
    transport: TcpTransport,
    deadline: Instant,
}

fn wait_socket(socket: &Mutex<DriverSocket>, deadline: Instant) -> Result<(), Error> {
    socket
        .lock()
        .map_err(|_| Error::Transport)?
        .transport
        .wait_readable(deadline)
        .map_err(|error| {
            if error == crate::ports::Error::Deadline {
                Error::Deadline
            } else {
                Error::Transport
            }
        })
}

struct Network {
    socket: Arc<Mutex<DriverSocket>>,
    activity: Arc<AtomicBool>,
}
impl Network {
    fn progress(
        &self,
        result: Result<IoProgress, crate::ports::Error>,
    ) -> Result<IoProgress, crate::ports::Error> {
        if matches!(result, Ok(IoProgress::Bytes(count)) if count > 0) {
            self.activity.store(true, Ordering::Relaxed);
        }
        result
    }
}
impl Transport for Network {
    fn read(&mut self, output: &mut [u8]) -> Result<IoProgress, crate::ports::Error> {
        let result = self
            .socket
            .lock()
            .map_err(|_| crate::ports::Error::Invalid)?
            .transport
            .read(output);
        self.progress(result)
    }
    fn write(&mut self, input: &[u8]) -> Result<IoProgress, crate::ports::Error> {
        let result = {
            let mut socket = self
                .socket
                .lock()
                .map_err(|_| crate::ports::Error::Invalid)?;
            let deadline = socket.deadline;
            socket.transport.write_until(input, deadline)
        };
        self.progress(result)
    }
    fn flush(&mut self) -> Result<FlushProgress, crate::ports::Error> {
        self.socket
            .lock()
            .map_err(|_| crate::ports::Error::Invalid)?
            .transport
            .flush()
    }
    fn close(&mut self) -> Result<FlushProgress, crate::ports::Error> {
        self.socket
            .lock()
            .map_err(|_| crate::ports::Error::Invalid)?
            .transport
            .close()
    }
    fn abort(&mut self) {
        if let Ok(mut socket) = self.socket.lock() {
            socket.transport.abort();
        }
    }
}

type Secure = TlsIo<Network, Box<[u8; TLS_WIRE_BYTES]>>;
struct Connector {
    session: TlsSession,
    clock: Arc<dyn Clock>,
    cap: Deadline,
    handshake_cap: Deadline,
    output_limit: usize,
    input: Input,
}

impl tls::TlsConnect<Io<Network>> for Connector {
    type Stream = Io<Secure>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Self::Stream>> + Send>>;

    fn connect(self, raw: Io<Network>) -> Self::Future {
        Box::pin(async move {
            let mut timing = crate::performance::Timer::start(crate::performance::Phase::DbTls);
            let mut secure = TlsIo::new(
                raw.transport,
                self.session,
                self.clock,
                self.cap,
                self.handshake_cap,
                Box::new([0; TLS_WIRE_BYTES]),
                Box::new([0; TLS_WIRE_BYTES]),
            )
            .map_err(|_| refused())?;
            poll_fn(|_| match secure.handshake() {
                Ok(Some(_)) => Poll::Ready(Ok(())),
                Ok(None) => Poll::Pending,
                Err(_) => Poll::Ready(Err(refused())),
            })
            .await?;
            timing.succeeded();
            let mut io = Io::new(secure, true);
            io.output_left = self.output_limit;
            io.input = Some(self.input);
            Ok(io)
        })
    }
}

impl tls::TlsStream for Io<Secure> {
    fn channel_binding(&self) -> tls::ChannelBinding {
        tls::ChannelBinding::none()
    }
}

// Private to this worker: read Pending never blocks a driver write. After
// a complete poll with no socket or channel progress, the owner waits for
// readability. No external reactor or concurrent operation uses this socket.
struct Io<T> {
    transport: T,
    input: Option<Input>,
    output_left: usize,
}
impl<T> Io<T> {
    fn new(transport: T, secured: bool) -> Self {
        Self {
            transport,
            input: secured.then(Input::new),
            output_left: 64 * 1024,
        }
    }
}
fn refused() -> io::Error {
    io::ErrorKind::InvalidData.into()
}
impl<T: Transport + Unpin> AsyncRead for Io<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if output.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let this = self.get_mut();
        let length = output.remaining().min(16 * 1024);
        let Some(bytes) = output.initialize_unfilled().get_mut(..length) else {
            return Poll::Ready(Err(refused()));
        };
        match this.transport.read(bytes) {
            Ok(IoProgress::Bytes(count)) => {
                let Some(received) = bytes.get(..count) else {
                    return Poll::Ready(Err(refused()));
                };
                if let Some(input) = &mut this.input {
                    if input.accept(received).is_err() {
                        this.transport.abort();
                        return Poll::Ready(Err(refused()));
                    }
                }
                output.advance(count);
                Poll::Ready(Ok(()))
            }
            Ok(IoProgress::Pending) => Poll::Pending,
            Ok(IoProgress::Closed) => Poll::Ready(Ok(())),
            Err(_) => Poll::Ready(Err(refused())),
        }
    }
}
impl<T: Transport + Unpin> AsyncWrite for Io<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if bytes.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if bytes.len() > this.output_left
            || this
                .input
                .as_ref()
                .is_some_and(|input| input.admit_output(bytes.len()).is_err())
        {
            if let Some(input) = &mut this.input {
                input.fail();
            }
            this.transport.abort();
            return Poll::Ready(Err(refused()));
        }
        match this.transport.write(bytes) {
            Ok(IoProgress::Bytes(count)) if count <= bytes.len() => {
                this.output_left -= count;
                if this
                    .input
                    .as_ref()
                    .is_some_and(|input| input.output(count).is_err())
                {
                    this.transport.abort();
                    return Poll::Ready(Err(refused()));
                }
                Poll::Ready(Ok(count))
            }
            Ok(IoProgress::Pending) => Poll::Pending,
            _ => Poll::Ready(Err(refused())),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        progress(self.get_mut().transport.flush())
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        progress(self.get_mut().transport.close())
    }
}
fn progress(result: Result<FlushProgress, crate::ports::Error>) -> Poll<io::Result<()>> {
    match result {
        Ok(FlushProgress::Complete) => Poll::Ready(Ok(())),
        Ok(FlushProgress::Pending) => Poll::Pending,
        Err(_) => Poll::Ready(Err(refused())),
    }
}

mod audit_budget;
pub(crate) use audit_budget::AuditBudget;
mod retained;
pub(crate) use retained::{ReadIsolation, ReadSession};
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
    use std::net::TcpListener;

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
    fn socket_activity_distinguishes_progress_from_idle_polling() {
        use std::io::{Read, Write};
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let activity = Arc::new(AtomicBool::new(false));
        let mut network = Network {
            socket: Arc::new(Mutex::new(DriverSocket {
                transport: TcpTransport::from_stream(socket).unwrap(),
                deadline: Instant::now() + Duration::from_secs(5),
            })),
            activity: activity.clone(),
        };
        let mut byte = [0];
        assert_eq!(network.read(&mut byte), Ok(IoProgress::Pending));
        assert_eq!(network.flush(), Ok(FlushProgress::Complete));
        assert!(!activity.load(Ordering::Relaxed));
        peer.write_all(b"x").unwrap();
        let stop = Instant::now() + Duration::from_secs(1);
        while network.read(&mut byte).unwrap() == IoProgress::Pending {
            assert!(!activity.load(Ordering::Relaxed));
            assert!(Instant::now() < stop);
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(byte, *b"x");
        assert!(activity.load(Ordering::Relaxed));
        activity.store(false, Ordering::Relaxed);
        assert_eq!(network.write(b"y"), Ok(IoProgress::Bytes(1)));
        assert!(activity.load(Ordering::Relaxed));
        peer.read_exact(&mut byte).unwrap();
        assert_eq!(byte, *b"y");
        activity.store(false, Ordering::Relaxed);
        assert_eq!(network.read(&mut byte), Ok(IoProgress::Pending));
        assert!(!activity.load(Ordering::Relaxed));
    }

    struct WindowTransport {
        pending: bool,
        calls: usize,
        aborted: bool,
    }
    impl Transport for WindowTransport {
        fn read(&mut self, _: &mut [u8]) -> Result<IoProgress, crate::ports::Error> {
            Ok(IoProgress::Pending)
        }
        fn write(&mut self, bytes: &[u8]) -> Result<IoProgress, crate::ports::Error> {
            self.calls += 1;
            if self.pending {
                self.pending = false;
                Ok(IoProgress::Pending)
            } else {
                Ok(IoProgress::Bytes(bytes.len().min(1)))
            }
        }
        fn flush(&mut self) -> Result<FlushProgress, crate::ports::Error> {
            Ok(FlushProgress::Complete)
        }
        fn close(&mut self) -> Result<FlushProgress, crate::ports::Error> {
            Ok(FlushProgress::Complete)
        }
        fn abort(&mut self) {
            self.aborted = true;
        }
    }

    #[test]
    fn retained_output_charges_actual_progress_and_refuses_before_transport() {
        let mut io = Io::new(
            WindowTransport {
                pending: true,
                calls: 0,
                aborted: false,
            },
            true,
        );
        io.input = Some(Input::validation_checks(
            ValidationBudget::test_turns(BODY_CHECK_TURNS as u64).unwrap(),
        ));
        io.output_left = BODY_CHECK_TURNS * 64 * 1024;
        let window = io.input.as_ref().unwrap().window().unwrap();
        window.begin().unwrap();
        window.output(64 * 1024 - 2).unwrap();
        let waker = Waker::from(Arc::new(WorkerWake(
            thread::current(),
            Arc::new(AtomicBool::new(false)),
        )));
        let mut context = Context::from_waker(&waker);
        assert!(Pin::new(&mut io)
            .poll_write(&mut context, b"ab")
            .is_pending());
        assert_eq!(io.output_left, BODY_CHECK_TURNS * 64 * 1024);
        assert!(matches!(
            Pin::new(&mut io).poll_write(&mut context, b"ab"),
            Poll::Ready(Ok(1))
        ));
        assert!(matches!(
            Pin::new(&mut io).poll_write(&mut context, b"b"),
            Poll::Ready(Ok(1))
        ));
        assert_eq!(io.output_left, BODY_CHECK_TURNS * 64 * 1024 - 2);
        window.finish().unwrap();
        window.begin().unwrap();
        window.output(64 * 1024 - 1).unwrap();
        assert!(matches!(
            Pin::new(&mut io).poll_write(&mut context, b"ab"),
            Poll::Ready(Err(_))
        ));
        assert_eq!(io.transport.calls, 3);
        assert!(io.transport.aborted);
        assert!(window.finish().is_err());
        assert!(window.begin().is_err());
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
            address,
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
}
