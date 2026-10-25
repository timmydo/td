//! Real fixed-worker SMTP ingress against the prepared PostgreSQL adapter.
use super::{sql, Fixture, PASSWORD};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{mpsc, Arc, Condvar, Mutex},
    time::{Duration, Instant},
};
use td_mta::{
    clock::{RuntimeClock, TlsClockSource},
    config::materialize::ResolvedProfile,
    generations::GenerationLease,
    ports::{Clock, Deadline, FlushProgress, IoProgress, Transport},
    postgres_read::ReadSlots,
    postgres_runtime_profile::Binding,
    postgres_smtp_receiving::{Pending, Receiving as Owner},
    postgres_smtp_storage::Backend,
    smtp_receiving::{BoundListener, Control, Receiving, State},
    smtp_storage::{self, Job, Storage},
    tls_admission::HandshakePool,
    tls_io::{TlsIo, TLS_WIRE_BYTES},
    tls_policy::TlsPolicies,
    transport::TcpTransport,
};

#[derive(Default)]
struct CopyState {
    entered: bool,
    observed: bool,
    released: bool,
}
#[derive(Default)]
struct CopyProbe {
    state: Mutex<CopyState>,
    changed: Condvar,
}
impl CopyProbe {
    fn wait(&self, condition: impl Fn(&CopyState) -> bool) -> bool {
        let until = Instant::now() + Duration::from_secs(5);
        let mut state = self.state.lock().unwrap();
        while !condition(&state) {
            let Some(remaining) = until.checked_duration_since(Instant::now()) else {
                return false;
            };
            state = self.changed.wait_timeout(state, remaining).unwrap().0;
        }
        true
    }
    fn release(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.released = true;
            self.changed.notify_all();
        }
    }
}
struct ReleaseCopy(Arc<CopyProbe>);
impl Drop for ReleaseCopy {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct ObservedStorage<'a, B> {
    inner: &'a B,
    probe: Arc<CopyProbe>,
}
struct ObservedJob<J> {
    inner: J,
    probe: Arc<CopyProbe>,
    gated: bool,
}
impl<B: Storage> Storage for ObservedStorage<'_, B> {
    fn maintain(&self) -> Result<(), td_mta::ports::Error> {
        self.inner.maintain()
    }
    fn close(&self) -> Result<(), td_mta::ports::Error> {
        self.inner.close()
    }

    type Job<'a>
        = ObservedJob<B::Job<'a>>
    where
        Self: 'a;
    fn capacity(&self) -> u64 {
        self.inner.capacity()
    }
    fn ready(
        &self,
        account: td_mta::ids::AccountId,
        deadline: Deadline,
    ) -> Result<(), td_mta::ports::Error> {
        self.inner.ready(account, deadline)
    }
    fn stopped(&self) -> bool {
        self.inner.stopped()
    }
    fn begin(
        &self,
        input: smtp_storage::Begin<'_, '_>,
        random: &mut td_crypto::SystemEntropy,
    ) -> Result<Self::Job<'_>, smtp_storage::Failure> {
        Ok(ObservedJob {
            inner: self.inner.begin(input, random)?,
            probe: self.probe.clone(),
            gated: false,
        })
    }
}
impl<J: Job> Job for ObservedJob<J> {
    fn write(&mut self, bytes: &[u8]) -> Result<(), smtp_storage::Failure> {
        self.inner.write(bytes)?;
        if bytes == b"Subject: held publication\r\n" {
            self.gated = true;
        }
        if bytes == b"Subject: independent DATA copy\r\n" {
            self.probe.state.lock().unwrap().observed = true;
            self.probe.changed.notify_all();
        }
        Ok(())
    }
    fn prepare(&mut self, deadline: Deadline) -> Result<(), smtp_storage::Failure> {
        self.inner.prepare(deadline)
    }
    fn commit(&mut self) -> Result<smtp_storage::Completion, smtp_storage::Failure> {
        if self.gated {
            self.probe.state.lock().unwrap().entered = true;
            self.probe.changed.notify_all();
            if !self.probe.wait(|state| state.released) {
                return Err(smtp_storage::Failure::Refused(
                    td_mta::ports::Error::Deadline,
                ));
            }
        }
        self.inner.commit()
    }
}

trait Duplex: Read + Write {}
impl<T: Read + Write> Duplex for T {}
pub(super) struct TlsDriver(TlsIo<TcpTransport, Box<[u8; TLS_WIRE_BYTES]>>);
fn io_error(error: td_mta::ports::Error) -> std::io::Error {
    let kind = match error {
        td_mta::ports::Error::Tls => std::io::ErrorKind::ConnectionAborted,
        td_mta::ports::Error::Deadline => std::io::ErrorKind::TimedOut,
        td_mta::ports::Error::Io { kind, .. } => kind,
        _ => std::io::ErrorKind::Other,
    };
    std::io::Error::new(kind, error)
}
fn progress<T>(mut poll: impl FnMut() -> std::io::Result<Option<T>>) -> std::io::Result<T> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if Instant::now() >= deadline {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        if let Some(result) = poll()? {
            return Ok(result);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}
impl Read for TlsDriver {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        progress(|| {
            Ok(
                match Transport::read(&mut self.0, bytes).map_err(io_error)? {
                    IoProgress::Bytes(n) => Some(n),
                    IoProgress::Pending => None,
                    IoProgress::Closed => Some(0),
                },
            )
        })
    }
}
impl Write for TlsDriver {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        progress(|| {
            Ok(
                match Transport::write(&mut self.0, bytes).map_err(io_error)? {
                    IoProgress::Bytes(n) => Some(n),
                    IoProgress::Pending => None,
                    IoProgress::Closed => Some(0),
                },
            )
        })
    }
    fn flush(&mut self) -> std::io::Result<()> {
        progress(|| {
            Ok(
                (Transport::flush(&mut self.0).map_err(io_error)? == FlushProgress::Complete)
                    .then_some(()),
            )
        })
    }
}
fn upgrade(stream: TcpStream, clock: Arc<RuntimeClock>, ca: &[u8]) -> (TlsDriver, i64) {
    upgrade_protocol(stream, clock, ca, td_crypto::TlsProtocol::Smtp)
}
pub(super) fn upgrade_protocol(
    stream: TcpStream,
    clock: Arc<RuntimeClock>,
    ca: &[u8],
    protocol: td_crypto::TlsProtocol,
) -> (TlsDriver, i64) {
    let trust = td_crypto::TrustStore::from_pem(ca).unwrap();
    let tls_clock = Arc::new(td_crypto::ClockHandle::new(TlsClockSource::new(
        clock.clone(),
    )));
    let config = Arc::new(td_crypto::ClientConfig::new(&trust, tls_clock, protocol).unwrap());
    let session = td_crypto::TlsSession::client(config, "localhost").unwrap();
    let now = clock.sample().unwrap().monotonic;
    let mut io = TlsIo::new(
        TcpTransport::from_stream(stream).unwrap(),
        session,
        clock,
        Deadline::after(now, 120_000).unwrap(),
        Deadline::after(now, 15_000).unwrap(),
        Box::new([0; TLS_WIRE_BYTES]),
        Box::new([0; TLS_WIRE_BYTES]),
    )
    .unwrap();
    let info = progress(|| io.handshake().map_err(io_error)).unwrap();
    assert_eq!(info.peer, td_crypto::PeerEvidence::VerifiedServerName);
    let version = match info.version {
        td_crypto::TlsVersion::V12 => 1,
        td_crypto::TlsVersion::V13 => 2,
    };
    (TlsDriver(io), version)
}

struct Stop<'a>(&'a Control);
impl Drop for Stop<'_> {
    fn drop(&mut self) {
        self.0.stop();
    }
}
fn reply(stream: &mut BufReader<impl Read>, code: &str) -> String {
    let mut response = String::new();
    for _ in 0..64 {
        let mut line = String::new();
        assert!(stream.read_line(&mut line).unwrap() > 0);
        assert!(line.starts_with(code), "unexpected SMTP reply {line:?}");
        assert!(line.len() <= 1024);
        response.push_str(&line);
        if line.as_bytes().get(3) == Some(&b' ') {
            return response;
        }
        assert_eq!(line.as_bytes().get(3), Some(&b'-'));
    }
    panic!("SMTP reply did not terminate")
}
fn command(stream: &mut BufReader<impl Read + Write>, line: &[u8], code: &str) {
    stream.get_mut().write_all(line).unwrap();
    stream.get_mut().flush().unwrap();
    reply(stream, code);
}
fn envelope(stream: &mut BufReader<impl Read + Write>) {
    command(stream, b"MAIL FROM:<sender@example.test>\r\n", "250");
    command(stream, b"RCPT TO:<main@example.test>\r\n", "250");
    command(stream, b"DATA\r\n", "354");
}

fn connect(
    fixture: &Fixture,
    address: std::net::SocketAddr,
    tls: bool,
    clock: Arc<RuntimeClock>,
) -> BufReader<Box<dyn Duplex>> {
    let stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    let mut client = BufReader::new(stream);
    reply(&mut client, "220");
    command(&mut client, b"EHLO sender.example.test\r\n", "250");
    let wire: Box<dyn Duplex> = if tls {
        command(&mut client, b"STARTTLS\r\n", "220");
        assert!(client.buffer().is_empty());
        let (driver, _) = upgrade(client.into_inner(), clock.clone(), &fixture.ca);
        Box::new(driver)
    } else {
        Box::new(client.into_inner())
    };
    let mut client = BufReader::new(wire);
    if tls {
        command(&mut client, b"EHLO sender.example.test\r\n", "250");
    }
    client
}

fn concurrent_data(
    fixture: &Fixture,
    address: std::net::SocketAddr,
    tls: bool,
    clock: Arc<RuntimeClock>,
) {
    let initial: u64 = sql(fixture, "SELECT count(*) FROM td_mta.receiving_receipts")
        .parse()
        .unwrap();
    let mut slow = connect(fixture, address, tls, clock.clone());
    envelope(&mut slow);
    slow.get_mut()
        .write_all(b"Subject: concurrent slow\r\n\r\npartial\r\n")
        .unwrap();
    slow.get_mut().flush().unwrap();
    // Keep the first sender in DATA until another sender has durably completed.
    let mut fast = connect(fixture, address, tls, clock);
    envelope(&mut fast);
    command(
        &mut fast,
        b"Subject: concurrent fast\r\n\r\nbody\r\n.\r\n",
        "250",
    );
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.receiving_receipts"),
        (initial + 1).to_string()
    );
    command(&mut slow, b".\r\n", "250");
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.receiving_receipts"),
        (initial + 2).to_string()
    );
    for subject in ["concurrent slow", "concurrent fast"] {
        assert_eq!(
            sql(fixture, &format!("SELECT count(*) FROM td_mta.blob_chunks WHERE pg_catalog.encode(bytes,'escape') LIKE '%Subject: {subject}%'")),
            "1"
        );
    }
    command(&mut slow, b"QUIT\r\n", "221");
    command(&mut fast, b"QUIT\r\n", "221");
}

fn copy_during_publication(
    fixture: &Fixture,
    address: std::net::SocketAddr,
    tls: bool,
    clock: Arc<RuntimeClock>,
    probe: Arc<CopyProbe>,
) {
    let release = ReleaseCopy(probe.clone());
    let initial: usize = sql(fixture, "SELECT count(*) FROM td_mta.receiving_receipts")
        .parse()
        .unwrap();
    let mut held = connect(fixture, address, tls, clock.clone());
    envelope(&mut held);
    let mut copying = connect(fixture, address, tls, clock);
    envelope(&mut copying);
    held.get_mut()
        .write_all(b"Subject: held publication\r\n\r\nbody\r\n.\r\n")
        .unwrap();
    held.get_mut().flush().unwrap();
    assert!(
        probe.wait(|state| state.entered),
        "publication did not reach gate"
    );
    copying
        .get_mut()
        .write_all(b"Subject: independent DATA copy\r\n\r\nbody\r\n")
        .unwrap();
    copying.get_mut().flush().unwrap();
    assert!(
        probe.wait(|state| state.observed),
        "DATA copy waited behind publication"
    );
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.receiving_receipts"),
        initial.to_string()
    );
    drop(release);
    reply(&mut held, "250");
    command(&mut copying, b".\r\n", "250");
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.receiving_receipts"),
        (initial + 2).to_string()
    );
    command(&mut held, b"QUIT\r\n", "221");
    command(&mut copying, b"QUIT\r\n", "221");
}

pub(super) fn exercise(
    fixture: &Fixture,
    binding: &Binding,
    configuration: &ResolvedProfile,
    policies: &GenerationLease<TlsPolicies>,
    socket: TcpListener,
    tls: bool,
) {
    let address = socket.local_addr().unwrap();
    let loaded = td_mta::postgres_input::Loaded {
        bootstrap: td_mta::postgres_bootstrap::Bootstrap::parse(
            &format!(
                "postgresql://mail_runtime:{}@localhost:{}/postgres",
                std::str::from_utf8(PASSWORD).unwrap(),
                fixture.port
            ),
            binding.profile(),
        )
        .unwrap(),
        trust: td_crypto::TrustStore::from_pem(&fixture.ca).unwrap(),
    };
    let clock = Arc::new(RuntimeClock::new());
    let slots = ReadSlots::new(1).unwrap();
    let pending = Pending::default();
    let storage = Backend::new(
        Owner {
            loaded: &loaded,
            executable: std::path::Path::new(env!("CARGO_BIN_EXE_td-mta")),
            binding,
            additional: &[],
            slots: &slots,
            clock: &clock,
            pending: &pending,
        },
        clock.clone(),
        policies.id().get(),
    )
    .unwrap();
    let probe = Arc::new(CopyProbe::default());
    let storage = ObservedStorage {
        inner: &storage,
        probe: probe.clone(),
    };
    let validated = configuration.candidate().resources();
    let pool = HandshakePool::new(validated.resources().limits().tls_handshakes).unwrap();
    configuration.candidate().with_graph(|records, graph| {
        let listeners = records.view(graph).unwrap().listeners().unwrap();
        let row = (0..listeners.len()).find_map(|index| {
            listeners.listener(index).unwrap().filter(|row| row.name == "smtp")
        }).unwrap();
        let bound = [BoundListener::new(socket, row, policies).unwrap()];
        let receiving = Receiving {
            storage: &storage, routes: records.routes(), resources: validated.resources(),
            admission: validated.admission(), timeouts: validated.timeouts(), policies,
            handshakes: &pool, listeners: &bound, clock: clock.clone(),
        };
        let control = Control::default();
        std::thread::scope(|scope| {
            let (tx, rx) = mpsc::sync_channel(1);
            let server = scope.spawn({
                let receiving = &receiving;
                let control = &control;
                move || receiving.run_with_ready(control, || {
                    tx.send(()).map_err(|_| td_mta::ports::Error::Invalid)
                })
            });
            let stop = Stop(&control);
            rx.recv_timeout(Duration::from_secs(20)).unwrap();
            let ready_deadline = Instant::now() + Duration::from_secs(5);
            while control.state() == State::Starting {
                assert!(Instant::now() < ready_deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(control.state(), State::Ready);
            let stream = TcpStream::connect(address).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(15))).unwrap();
            let mut client = BufReader::new(stream);
            reply(&mut client, "220");
            command(
                &mut client,
                if tls { b"EHLO before.example.test\r\n" } else { b"EHLO sender.example.test\r\n" },
                "250",
            );
            let (wire, version): (Box<dyn Duplex>, i64) = if tls {
                command(&mut client, b"MAIL FROM:<discard@example.test>\r\n", "250");
                command(&mut client, b"STARTTLS\r\n", "503");
                command(&mut client, b"RSET\r\n", "250");
                command(&mut client, b"STARTTLS\r\n", "220");
                assert!(client.buffer().is_empty());
                let (driver, version) = upgrade(client.into_inner(), clock.clone(), &fixture.ca);
                (Box::new(driver), version)
            } else {
                assert!(client.buffer().is_empty());
                (Box::new(client.into_inner()), 0)
            };
            let mut client = BufReader::new(wire);
            if tls {
                command(&mut client, b"MAIL FROM:<sender@example.test>\r\n", "503");
                command(&mut client, b"EHLO sender.example.test\r\n", "250");
                command(&mut client, b"RCPT TO:<main@example.test>\r\n", "503");
            }
            // Receiving never grants relay authority, even for a forged local sender.
            command(&mut client, b"AUTH PLAIN AGxvY2FsAHBhc3N3b3Jk\r\n", "500");
            for sender in ["outside@remote.test", "main@example.test", ""] {
                command(&mut client, format!("MAIL FROM:<{sender}>\r\n").as_bytes(), "250");
                for recipient in ["outside@remote.test", "unknown@example.test",
                    "outside%remote.test@example.test", "remote.test!outside@example.test",
                    "@example.test:outside@remote.test"] {
                    command(&mut client, format!("RCPT TO:<{recipient}>\r\n").as_bytes(), "550");
                }
                command(&mut client, b"DATA\r\n", "503");
                command(&mut client, b"RSET\r\n", "250");
            }
            envelope(&mut client);
            client.get_mut().write_all(b"Subject: PostgreSQL worker\r\nFrom: main@example.test\r\nTo: outside@remote.test\r\nResent-To: other@remote.test\r\nX-Original-To: third@remote.test\r\nAuthentication-Results: example.test; auth=pass\r\n\r\nbody\r\n").unwrap();
            assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.receiving_receipts"), "0");
            command(&mut client, b".\r\n", "250");
            assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.receiving_receipts"), "1");
            assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "1");
            assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.submissions"), "0");
            assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.submission_routes"), "0");
            assert_eq!(pending.operation().unwrap(), None);

            // A known rollback refuses the transaction without retiring the worker.
            sql(fixture, "CREATE FUNCTION public.reject_wire_receiving() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'reject'; END $$; CREATE TRIGGER reject_wire_receiving BEFORE INSERT ON td_mta.receiving_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_wire_receiving()");
            envelope(&mut client);
            command(&mut client, b"Subject: refused\r\n\r\nbody\r\n.\r\n", "451");
            assert_eq!(control.state(), State::Ready);
            assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.receiving_receipts"), "1");
            sql(fixture, "DROP TRIGGER reject_wire_receiving ON td_mta.receiving_receipts");
            envelope(&mut client);
            command(&mut client, b"Subject: after refusal\r\n\r\nbody\r\n.\r\n", "250");
            assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.receiving_receipts"), "2");

            concurrent_data(fixture, address, tls, clock.clone());
            copy_during_publication(fixture, address, tls, clock.clone(), probe.clone());

            // Shutdown discards a partially received message and joins the workers.
            envelope(&mut client);
            client.get_mut().write_all(b"Subject: abandoned\r\n\r\npartial\r\n").unwrap();
            client.get_mut().flush().unwrap();
            let mut abandoned = Vec::new();
            for _ in 1..td_mta::limits::SMTP_DATA_PER_PEER {
                let mut extra = connect(fixture, address, tls, clock.clone());
                envelope(&mut extra);
                extra.get_mut().write_all(b"Subject: abandoned sibling\r\n\r\npartial\r\n").unwrap();
                extra.get_mut().flush().unwrap();
                abandoned.push(extra);
            }
            let mut excess = connect(fixture, address, tls, clock.clone());
            command(&mut excess, b"MAIL FROM:<sender@example.test>\r\n", "250");
            command(&mut excess, b"RCPT TO:<main@example.test>\r\n", "250");
            command(&mut excess, b"DATA\r\n", "451");
            assert_eq!(control.state(), State::Ready);
            drop(excess);
            drop(stop);
            server.join().unwrap().unwrap();
            drop(abandoned);
            match client.read(&mut [0]) {
                Ok(0) => (),
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => (),
                Err(error) if tls && error.kind() == std::io::ErrorKind::ConnectionAborted => (),
                other => panic!("unfinished DATA transport stayed open: {other:?}"),
            }
            drop(client);
            assert_eq!(control.state(), State::Stopped);
            assert_eq!(pending.operation().unwrap(), None);
            assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.receiving_receipts"), "6");
            assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "6");
            assert_eq!(sql(fixture, &format!("SELECT count(*) FROM td_mta.emails WHERE tls={version} AND ehlo='sender.example.test' AND reverse_path='sender@example.test'")), "6");
        });
    }).unwrap();
}
