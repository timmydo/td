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
    sync::Arc,
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid,
    Deadline,
    Transport,
    Tls,
    Database,
    Settings,
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
        64 * 1024,
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
    let allowance = remaining(deadline)?;
    if allowance > MAX_DURATION
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
    let milliseconds = u64::try_from(allowance.as_millis()).map_err(|_| Error::Invalid)?;
    let cap = Deadline::after(now, milliseconds).map_err(|_| Error::Invalid)?;
    let tls_clock = Arc::new(ClockHandle::new(TlsClockSource::new(clock.clone())));
    let policy = Arc::new(
        ClientConfig::new(trust, tls_clock, TlsProtocol::Postgres).map_err(|_| Error::Tls)?,
    );
    let session = TlsSession::client(policy, request.server_name).map_err(|_| Error::Tls)?;
    let socket = TcpStream::connect_timeout(&request.address, remaining(deadline)?)
        .map_err(|_| Error::Transport)?;
    let transport = TcpTransport::from_stream(socket).map_err(|_| Error::Transport)?;
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
    let connector = Connector {
        session,
        clock,
        cap,
        output_limit,
        input,
    };
    let waker = Waker::from(Arc::new(WorkerWake(thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut connect = pin!(config.connect_raw(Io::new(transport, false), connector));
    let (client, connection) = loop {
        remaining(deadline)?;
        if let Poll::Ready(result) = connect.as_mut().poll(&mut context) {
            break result.map_err(|_| Error::Database)?;
        }
        thread::park_timeout(Duration::from_millis(1));
    };
    let mut connection = pin!(connection);
    let mut operation = operation(&client);
    loop {
        remaining(deadline)?;
        if let Poll::Ready(result) = operation.as_mut().poll(&mut context) {
            return Ok(result);
        }
        match connection.as_mut().poll_message(&mut context) {
            Poll::Ready(Some(Ok(_))) | Poll::Pending => {}
            Poll::Ready(Some(Err(_))) | Poll::Ready(None) => return Err(Error::Database),
        }
        thread::park_timeout(Duration::from_millis(1));
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

fn remaining(deadline: Instant) -> Result<Duration, Error> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or(Error::Deadline)
}

struct WorkerWake(thread::Thread);
impl Wake for WorkerWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

type Secure = TlsIo<TcpTransport, Box<[u8; TLS_WIRE_BYTES]>>;
struct Connector {
    session: TlsSession,
    clock: Arc<dyn Clock>,
    cap: Deadline,
    output_limit: usize,
    input: Input,
}

impl tls::TlsConnect<Io<TcpTransport>> for Connector {
    type Stream = Io<Secure>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Self::Stream>> + Send>>;

    fn connect(self, raw: Io<TcpTransport>) -> Self::Future {
        Box::pin(async move {
            let mut secure = TlsIo::new(
                raw.transport,
                self.session,
                self.clock,
                self.cap,
                self.cap,
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

// Private to the periodically polled worker above: socket Pending does not
// register an external reactor. Driver channel wakeups unpark this worker.
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
        if bytes.len() > this.output_left {
            this.transport.abort();
            return Poll::Ready(Err(refused()));
        }
        match this.transport.write(bytes) {
            Ok(IoProgress::Bytes(count)) if count <= bytes.len() => {
                this.output_left -= count;
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

mod framing;
use framing::Input;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::net::TcpListener;

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
