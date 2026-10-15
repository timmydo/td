//! Real fixed-worker SMTP ingress against the prepared PostgreSQL adapter.
use super::{sql, Fixture, PASSWORD};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{mpsc, Arc},
    time::{Duration, Instant},
};
use td_mta::{
    clock::RuntimeClock,
    config::materialize::ResolvedProfile,
    generations::GenerationLease,
    postgres_read::ReadSlots,
    postgres_runtime_profile::Binding,
    postgres_smtp_receiving::{Pending, Receiving as Owner},
    postgres_smtp_storage::Backend,
    smtp_receiving::{BoundListener, Control, Receiving, State},
    tls_admission::HandshakePool,
    tls_policy::TlsPolicies,
};

struct Stop<'a>(&'a Control);
impl Drop for Stop<'_> {
    fn drop(&mut self) {
        self.0.stop();
    }
}
fn reply(stream: &mut BufReader<TcpStream>, code: &str) -> String {
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
fn command(stream: &mut BufReader<TcpStream>, line: &[u8], code: &str) {
    stream.get_mut().write_all(line).unwrap();
    reply(stream, code);
}
fn envelope(stream: &mut BufReader<TcpStream>) {
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
            command(&mut client, b"EHLO sender.example.test\r\n", "250");
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
            drop(stop);
            server.join().unwrap().unwrap();
            match client.read(&mut [0]) {
                Ok(0) => (),
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => (),
                other => panic!("unfinished DATA transport stayed open: {other:?}"),
            }
            drop(client);
            assert_eq!(control.state(), State::Stopped);
            assert_eq!(pending.operation().unwrap(), None);
            assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.receiving_receipts"), "2");
            assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "2");
        });
    }).unwrap();
}
