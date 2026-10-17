//! Real fixed-worker SMTP ingress against the prepared PostgreSQL adapter.
use super::{sql, Fixture, PASSWORD};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{mpsc, Arc},
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
    tls_admission::HandshakePool,
    tls_io::{TlsIo, TLS_WIRE_BYTES},
    tls_policy::TlsPolicies,
    transport::TcpTransport,
};

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
            slots: &slots,
            clock: &clock,
            pending: &pending,
        },
        clock.clone(),
        policies.id().get(),
    )
    .unwrap();
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
            envelope(&mut client);
            client.get_mut().write_all(b"Subject: PostgreSQL worker\r\n\r\nbody\r\n").unwrap();
            assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.receiving_receipts"), "0");
            command(&mut client, b".\r\n", "250");
            assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.receiving_receipts"), "1");
            assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "1");
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

            // Shutdown discards a partially received message and joins the workers.
            envelope(&mut client);
            client.get_mut().write_all(b"Subject: abandoned\r\n\r\npartial\r\n").unwrap();
            client.get_mut().flush().unwrap();
            drop(stop);
            server.join().unwrap().unwrap();
            match client.read(&mut [0]) {
                Ok(0) => (),
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => (),
                Err(error) if tls && error.kind() == std::io::ErrorKind::ConnectionAborted => (),
                other => panic!("unfinished DATA transport stayed open: {other:?}"),
            }
            drop(client);
            assert_eq!(control.state(), State::Stopped);
            assert_eq!(pending.operation().unwrap(), None);
            assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.receiving_receipts"), "2");
            assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "2");
            assert_eq!(sql(fixture, &format!("SELECT count(*) FROM td_mta.emails WHERE tls={version} AND ehlo='sender.example.test' AND reverse_path='sender@example.test'")), "2");
        });
    }).unwrap();
}
