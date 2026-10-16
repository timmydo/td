//! Paired authenticated TLS for the native outgoing fixture.
use super::Fixture;
use std::{
    fs,
    io::Cursor,
    net::{TcpListener, TcpStream},
    sync::Arc,
};
use td_mta::{
    admission::work::{Charge, Meter},
    clock::{RuntimeClock, TlsClockSource},
    config::{load, materialize, stanza, storage, stream},
    generations::GenerationSet,
    ports::{self, Clock, Deadline, FlushProgress, Handshake, IoProgress, TlsTransport, Transport},
    smtp_relay_auth as auth, smtp_relay_command as command, smtp_relay_ehlo as ehlo,
    tls_admission::HandshakePool,
    tls_io::TLS_WIRE_BYTES,
    tls_policy::{MaterialKind, TlsConnection, TlsPolicies},
    transport::TcpTransport,
};
pub(super) type Buffer = Box<[u8; TLS_WIRE_BYTES]>;
pub(super) type Policies = td_mta::generations::GenerationLease<TlsPolicies>;
pub(super) type Peer = TlsConnection<Buffer>;
pub(super) fn meter(cap: Deadline) -> Meter {
    Meter::new(
        cap,
        Charge {
            io_bytes: 64 * 1024 * 1024,
            output_bytes: 64 * 1024 * 1024,
            records: 100_000,
            unlinks: 0,
        },
    )
}
pub(super) fn until(clock: &RuntimeClock) -> Deadline {
    // The Prepared worker must retain its shorter ten-second admission cap.
    Deadline::after(clock.sample().unwrap().monotonic, 20_000).unwrap()
}
struct Script<'a> {
    expected: &'a [u8],
    reply: &'a [u8],
    received: Vec<u8>,
    sent: usize,
    flushed: bool,
}
impl<'a> Script<'a> {
    fn new(expected: &'a [u8], reply: &'a [u8]) -> Self {
        Self {
            expected,
            reply,
            received: Vec::new(),
            sent: 0,
            flushed: false,
        }
    }
    fn turn(&mut self, peer: &mut Peer) {
        let mut bytes = [0; 512];
        if let IoProgress::Bytes(n) = peer.read(&mut bytes).unwrap() {
            self.received.extend_from_slice(&bytes[..n]);
        }
        assert!(self.expected.starts_with(&self.received));
        if self.received == self.expected {
            if self.sent < self.reply.len() {
                if let IoProgress::Bytes(n) = peer.write(&self.reply[self.sent..]).unwrap() {
                    self.sent += n;
                }
            }
            self.flushed =
                peer.flush().unwrap() == FlushProgress::Complete && self.sent == self.reply.len();
        }
    }
    fn complete(&self) {
        assert!(self.flushed);
        assert_eq!(self.received, self.expected);
        assert_eq!(self.sent, self.reply.len());
    }
}
pub(super) fn authenticated(
    fixture: &Fixture,
    clock: Arc<RuntimeClock>,
    cap: Deadline,
    policies: &Policies,
) -> (auth::Session<Buffer>, Peer) {
    let (result, peer) = authenticate(fixture, clock, cap, Some(policies), false, false);
    (result.ok().unwrap(), peer)
}
pub(super) fn mismatched(
    fixture: &Fixture,
    clock: Arc<RuntimeClock>,
    cap: Deadline,
    rotated: bool,
) -> (auth::Session<Buffer>, Peer) {
    let (result, peer) = authenticate(fixture, clock, cap, None, rotated, false);
    (result.ok().unwrap(), peer)
}
pub(super) fn refused(
    fixture: &Fixture,
    clock: Arc<RuntimeClock>,
    cap: Deadline,
    policies: &Policies,
) -> (auth::Refusal<Buffer>, Peer) {
    let (result, peer) = authenticate(fixture, clock, cap, Some(policies), false, true);
    (result.err().unwrap(), peer)
}
fn authenticate(
    fixture: &Fixture,
    clock: Arc<RuntimeClock>,
    cap: Deadline,
    policies: Option<&Policies>,
    rotated: bool,
    refuse: bool,
) -> (Result<auth::Session<Buffer>, auth::Refusal<Buffer>>, Peer) {
    let password = if rotated {
        "rotated-password"
    } else {
        "database-password"
    };
    let lease = if let Some(policies) = policies {
        policies.clone()
    } else {
        let source = r#"version = 1
[server]
hostname = "localhost"
jmap_origin = "https://localhost"
[account "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
username = "user"
[domain "example.test"]
mx_host = "localhost"
[alias "main@example.test"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[identity "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
email = "main@example.test"
[resolver "primary"]
address = "127.0.0.1:53"
[relay]
route = "personal-smarthost"
host = "localhost"
port = 465
username = "relay-user"
password_file = "/password"
ca_file = "/ca"
[certificate "public"]
mode = "files"
chain_file = "/chain"
key_file = "/key"
[listener "smtp"]
kind = "direct_smtp"
bind = "127.0.0.1:25"
server_name = "localhost"
certificate = "public"
session_limit = 1
per_peer_limit = 1
[listener "https"]
kind = "https"
bind = "127.0.0.1:443"
certificate = "public"
"#;
        let mut scratch = vec![0; stream::SCRATCH_BYTES];
        let loaded = load::read(
            storage::Storage::try_new().unwrap(),
            &mut stanza::Pending::new(),
            &mut scratch,
            &mut source.as_bytes(),
        )
        .unwrap();
        let resolved = materialize::read_text(loaded, &mut scratch, |_| {
            Ok::<_, ports::Error>(Cursor::new(password.as_bytes()))
        })
        .unwrap();
        let chain = fs::read(fixture.root.join("server.crt")).unwrap();
        let key = fs::read(fixture.root.join("server.key")).unwrap();
        let mut set = GenerationSet::at_startup();
        let prepared = TlsPolicies::prepare(
            set.reserve().unwrap(),
            &resolved,
            Arc::new(td_crypto::ClockHandle::new(TlsClockSource::new(
                clock.clone(),
            ))),
            |request| {
                Ok::<_, ports::Error>(Cursor::new(match request.kind() {
                    MaterialKind::Chain => chain.as_slice(),
                    MaterialKind::Key => key.as_slice(),
                    _ => fixture.ca.as_slice(),
                }))
            },
        )
        .unwrap();
        drop(set.publish(prepared).unwrap());
        set.current().unwrap()
    };
    let pool = HandshakePool::new(2).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let left = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (right, _) = listener.accept().unwrap();
    let make = |socket, id| {
        TlsPolicies::reserve_session(
            lease.clone(),
            id,
            &pool,
            Box::new([0; TLS_WIRE_BYTES]),
            Box::new([0; TLS_WIRE_BYTES]),
        )
        .unwrap()
        .construct()
        .unwrap()
        .handoff(
            TcpTransport::from_stream(socket).unwrap(),
            &[],
            clock.clone(),
            cap,
            cap,
        )
        .unwrap()
    };
    let mut client = make(left, TlsPolicies::relay(&lease).unwrap());
    let mut peer = make(right, TlsPolicies::listener(&lease, "smtp").unwrap());
    let mut complete = false;
    for _ in 0..100_000 {
        if matches!(
            (client.handshake(cap).unwrap(), peer.handshake(cap).unwrap()),
            (Handshake::Complete(_), Handshake::Complete(_))
        ) {
            complete = true;
            break;
        }
        std::thread::yield_now();
    }
    assert!(complete);
    let mut greeting = Script::new(b"", b"220 ready\r\n");
    for _ in 0..100_000 {
        greeting.turn(&mut peer);
        if greeting.flushed {
            break;
        }
        std::thread::yield_now();
    }
    greeting.complete();
    let (mut input, mut first, mut second, mut outgoing) = ([0; 512], [0; 512], [0; 512], [0; 512]);
    let mut exchange = ehlo::Exchange::new(
        client,
        "localhost",
        ehlo::Scratch {
            input: &mut input,
            greeting: &mut first,
            reply: &mut second,
            command: &mut outgoing,
        },
        clock.clone(),
        cap,
    )
    .ok()
    .unwrap();
    let mut script = Script::new(
        b"EHLO localhost\r\n",
        b"250-localhost\r\n250 AUTH PLAIN\r\n",
    );
    let mut turns = 0;
    let session = loop {
        turns += 1;
        assert!(turns <= 100_000);
        script.turn(&mut peer);
        match exchange.advance().ok().unwrap() {
            ehlo::Progress::Pending(next) => exchange = next,
            ehlo::Progress::Ready(session) => break session,
        }
        std::thread::yield_now();
    };
    script.complete();
    let (mut third, mut plain, mut encoded) =
        ([0; 512], [0; auth::PLAIN_BYTES], [0; auth::RESPONSE_BYTES]);
    let mut work = meter(cap);
    let mut exchange = auth::Exchange::new(
        session,
        "relay-user",
        password,
        auth::Scratch {
            input: &mut input,
            first_reply: &mut first,
            second_reply: &mut second,
            third_reply: &mut third,
            plain: &mut plain,
            encoded: &mut encoded,
        },
        clock,
        cap,
        &mut work,
    )
    .ok()
    .unwrap();
    let mut scripts = [
        Script::new(b"AUTH PLAIN\r\n", b"334 \r\n"),
        Script::new(
            if rotated {
                b"AHJlbGF5LXVzZXIAcm90YXRlZC1wYXNzd29yZA==\r\n".as_slice()
            } else {
                b"AHJlbGF5LXVzZXIAZGF0YWJhc2UtcGFzc3dvcmQ=\r\n".as_slice()
            },
            if refuse {
                b"535 credentials refused\r\n".as_slice()
            } else {
                b"235 authenticated\r\n".as_slice()
            },
        ),
    ];
    let mut round = 0;
    turns = 0;
    let session = loop {
        turns += 1;
        assert!(turns <= 100_000);
        if let Some(script) = scripts.get_mut(round) {
            script.turn(&mut peer);
            if script.flushed {
                script.complete();
                round += 1;
            }
        }
        match exchange.advance() {
            Ok(auth::Progress::Pending(next)) => exchange = next,
            Ok(auth::Progress::Ready(session)) => break Ok(session),
            Err(error) => break Err(error),
        }
        std::thread::yield_now();
    };
    assert_eq!(round, 2);
    (session, peer)
}
pub(super) fn command(
    session: auth::Session<Buffer>,
    peer: &mut Peer,
    clock: Arc<RuntimeClock>,
    cap: Deadline,
    value: command::Command<'_>,
    expected: &[u8],
    reply: &[u8],
) -> (auth::Session<Buffer>, String) {
    let (mut input, mut line, mut text, mut outgoing) = ([0; 512], [0; 512], [0; 4096], [0; 512]);
    let mut work = meter(cap);
    let mut exchange = command::Exchange::new(
        session,
        value,
        command::Scratch {
            input: &mut input,
            line: &mut line,
            reply: &mut text,
            command: &mut outgoing,
        },
        clock,
        cap,
        &mut work,
    )
    .ok()
    .unwrap();
    let mut script = Script::new(expected, reply);
    for _ in 0..100_000 {
        script.turn(peer);
        match exchange.advance().ok().unwrap() {
            command::Progress::Pending(next) => exchange = next,
            command::Progress::Ready(reply) => {
                script.complete();
                let text = reply.text().unwrap().to_owned();
                return (reply.into_session(), text);
            }
        }
        std::thread::yield_now();
    }
    panic!("native relay command did not finish");
}

/// Drive bound MAIL and return its same-session positive handoff.
pub(super) fn accepted_mail<'w, 'a, 'r>(
    plan: td_mta::postgres_dispatch::MailPlan<'w, 'a, 'r>,
    session: auth::Session<Buffer>,
    peer: &mut Peer,
    cap: Deadline,
    database: &mut Meter,
) -> td_mta::postgres_relay_mail::Accepted<'w, 'a, 'r, Buffer> {
    use td_mta::{postgres_relay_mail as mail, smtp_data_encode::Body};
    let expected = format!("MAIL FROM:<{}>\r\n", plan.envelope().reverse_path);
    let mut input = [0; 512];
    let mut line = [0; 512];
    let mut reply = [0; 4096];
    let mut output = [0; 512];
    let mut network = meter(cap);
    let mut exchange = mail::begin(
        plan,
        session,
        Body::SevenBit,
        command::Scratch {
            input: &mut input,
            line: &mut line,
            reply: &mut reply,
            command: &mut output,
        },
        &mut network,
        database,
    )
    .ok()
    .unwrap();
    let mut script = Script::new(expected.as_bytes(), b"250 sender\r\n");
    loop {
        script.turn(peer);
        match exchange.advance() {
            Ok(mail::Progress::Pending(next)) => exchange = next,
            Ok(mail::Progress::Accepted(accepted)) => {
                script.complete();
                return accepted;
            }
            Ok(mail::Progress::Rejected(_)) => panic!("unexpected MAIL rejection"),
            Err(failure) => panic!("bound MAIL failed: {:?}", failure.error),
        }
        std::thread::yield_now();
    }
}

use td_mta::postgres_relay_rcpt as rcpt;
#[allow(
    clippy::large_enum_variant,
    reason = "Fixture keeps real TLS owners inline."
)]
pub(super) enum RcptStart<'w, 'a, 'r> {
    Mail(td_mta::postgres_relay_mail::Accepted<'w, 'a, 'r, Buffer>),
    Cursor(rcpt::Cursor<'w, 'a, 'r, Buffer>),
}
pub(super) enum RcptStep<'w, 'a, 'r> {
    Next(rcpt::Cursor<'w, 'a, 'r, Buffer>),
    Ready(rcpt::Completed<'w, 'a, 'r, Buffer>),
    Closed(rcpt::Closed<'w, 'a, 'r, Buffer>),
    Refused(rcpt::Refusal<'w, 'a, 'r, Buffer>),
}
pub(super) fn rcpt_command<'w, 'a, 'r>(
    start: RcptStart<'w, 'a, 'r>,
    peer: &mut Peer,
    cap: td_mta::ports::Deadline,
    database: &mut td_mta::admission::work::Meter,
    expected: &[u8],
    response: &[u8],
) -> (RcptStep<'w, 'a, 'r>, Vec<u8>) {
    let mut input = [0; 512];
    let mut line = [0; 512];
    let mut reply = [0; 4096];
    let mut output = [0; 512];
    let mut network = meter(cap);
    let scratch = td_mta::smtp_relay_command::Scratch {
        input: &mut input,
        line: &mut line,
        reply: &mut reply,
        command: &mut output,
    };
    let begin = match start {
        RcptStart::Mail(accepted) => rcpt::begin(accepted, scratch, &mut network, database),
        RcptStart::Cursor(cursor) => cursor.begin_next(scratch, &mut network, database),
    };
    let mut exchange = match begin {
        Ok(exchange) => exchange,
        Err(failure) => return (RcptStep::Refused(failure), Vec::new()),
    };
    let mut received = Vec::new();
    let mut sent = 0;
    loop {
        let mut bytes = [0; 512];
        if let td_mta::ports::IoProgress::Bytes(n) = peer.read(&mut bytes).unwrap() {
            received.extend_from_slice(&bytes[..n]);
        }
        assert!(expected.starts_with(&received));
        if received == expected {
            if sent < response.len() {
                if let td_mta::ports::IoProgress::Bytes(n) = peer.write(&response[sent..]).unwrap()
                {
                    sent += n;
                }
            }
            let _ = peer.flush().unwrap();
        }
        let step = match exchange.advance() {
            Ok(rcpt::Progress::Pending(next)) => {
                exchange = next;
                std::thread::yield_now();
                continue;
            }
            Ok(rcpt::Progress::Next(cursor)) => RcptStep::Next(cursor),
            Ok(rcpt::Progress::Ready(done)) => RcptStep::Ready(done),
            Ok(rcpt::Progress::Closed(closed)) => RcptStep::Closed(closed),
            Err(failure) => RcptStep::Refused(failure),
        };
        if !matches!(step, RcptStep::Refused(_)) {
            assert_eq!(received, expected);
            assert_eq!(sent, response.len());
        }
        return (step, received);
    }
}

/// Exercise one positive recipient through the bound envelope stages.
pub(super) fn accepted_body<'w, 'a, 'r>(
    plan: td_mta::postgres_dispatch::MailPlan<'w, 'a, 'r>,
    session: auth::Session<Buffer>,
    peer: &mut Peer,
    cap: Deadline,
    database: &mut Meter,
) -> td_mta::postgres_relay_data::Completed<'w, 'a, 'r, Buffer> {
    use td_mta::postgres_relay_data as data;
    assert_eq!(plan.envelope().recipients.len(), 1);
    let recipient = plan.envelope().recipients.first().unwrap();
    assert_eq!(recipient.ordinal, 0);
    let expected = format!("RCPT TO:<{}>\r\n", recipient.address);
    let accepted = accepted_mail(plan, session, peer, cap, database);
    let (step, received) = rcpt_command(
        RcptStart::Mail(accepted),
        peer,
        cap,
        database,
        expected.as_bytes(),
        b"250 recipient\r\n",
    );
    assert_eq!(received, expected.as_bytes());
    let RcptStep::Ready(completed) = step else {
        panic!("single positive RCPT did not complete");
    };
    let mut input = [0; 512];
    let mut line = [0; 512];
    let mut reply = [0; 4096];
    let mut output = [0; 512];
    let mut network = meter(cap);
    let mut exchange = data::begin(
        completed,
        command::Scratch {
            input: &mut input,
            line: &mut line,
            reply: &mut reply,
            command: &mut output,
        },
        &mut network,
        database,
    )
    .ok()
    .unwrap();
    let mut script = Script::new(b"DATA\r\n", b"354 send body\r\n");
    for _ in 0..100_000 {
        script.turn(peer);
        match exchange.advance() {
            Ok(data::Progress::Pending(next)) => exchange = next,
            Ok(data::Progress::Ready(completed)) => {
                script.complete();
                return completed;
            }
            Ok(data::Progress::Rejected(_)) => panic!("unexpected DATA rejection"),
            Err(failure) => panic!("bound DATA failed: {:?}", failure.error),
        }
        std::thread::yield_now();
    }
    panic!("bound DATA did not finish");
}
