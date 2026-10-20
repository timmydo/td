use super::*;
use crate::{
    ports::{Deadline, FlushProgress, IoProgress, Transport},
    smtp_relay_auth::{self as auth, Exchange, Progress, Refusal, Scratch, Session},
    smtp_relay_ehlo,
    smtp_wire::LINE_BYTES,
};
use td_mime::work::{Charge, Meter};

type Connection = TlsConnection<Box<[u8; TLS_WIRE_BYTES]>>;
type Ready = Session<Box<[u8; TLS_WIRE_BYTES]>>;
type Failed = Refusal<Box<[u8; TLS_WIRE_BYTES]>>;
fn cap() -> Deadline {
    Deadline::after(Tick(0), 1000).unwrap()
}
fn meter() -> Meter {
    Meter::new(
        cap(),
        Charge {
            io_bytes: 64 * 1024 * 1024,
            output_bytes: 8192,
            records: 100_000,
            unlinks: 0,
        },
    )
}
fn ready(
    material: &Material,
    advertisement: &[u8],
) -> (
    smtp_relay_ehlo::Session<Box<[u8; TLS_WIRE_BYTES]>>,
    Connection,
) {
    ready_credentials(material, advertisement, "private-relay", "password")
}
fn ready_credentials(
    material: &Material,
    advertisement: &[u8],
    username: &str,
    password: &str,
) -> (
    smtp_relay_ehlo::Session<Box<[u8; TLS_WIRE_BYTES]>>,
    Connection,
) {
    let configuration = {
        let _lock = text::TEST_CONSTRUCTION_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let source = source(false).replace("private-relay", username);
        let mut scratch = vec![0; stream::SCRATCH_BYTES];
        let loaded = load::read(
            storage::Storage::try_new().unwrap(),
            &mut stanza::Pending::new(),
            &mut scratch,
            &mut Cursor::new(source.as_bytes()),
        )
        .unwrap();
        materialize::read_text(loaded, &mut scratch, |_| {
            Ok::<_, Error>(Cursor::new(password.as_bytes()))
        })
        .unwrap()
    };
    let mut set = GenerationSet::at_startup();
    let prepared = TlsPolicies::prepare(
        set.reserve().unwrap(),
        &configuration,
        material.clock.clone(),
        |request| material.open(request),
    )
    .unwrap();
    drop(set.publish(prepared).unwrap());
    let lease = set.current().unwrap();
    let pool = HandshakePool::new(2).unwrap();
    let (mut client, mut peer) = io_tests::pair(material, &lease, &pool, "smtp");
    io_tests::complete(&mut client, &mut peer).unwrap();
    relay_ehlo_tests::send(&mut peer, b"220 ready\r\n");
    let (mut input, mut greeting, mut reply, mut command) =
        ([0; 512], [0; 512], [0; 512], [0; 512]);
    let exchange = smtp_relay_ehlo::Exchange::new(
        client,
        "mx.example.test",
        smtp_relay_ehlo::Scratch {
            input: &mut input,
            greeting: &mut greeting,
            reply: &mut reply,
            command: &mut command,
        },
        material.source.clone(),
        cap(),
    )
    .ok()
    .unwrap();
    let session = relay_ehlo_tests::drive(exchange, &mut peer, advertisement, 512, false)
        .ok()
        .unwrap();
    (session, peer)
}
struct Cells {
    input: [u8; 512],
    first: [u8; 512],
    second: [u8; 512],
    third: [u8; 512],
    plain: [u8; auth::PLAIN_BYTES],
    encoded: [u8; auth::RESPONSE_BYTES],
}
impl Cells {
    fn new() -> Self {
        Self {
            input: [0; 512],
            first: [0; 512],
            second: [0; 512],
            third: [0; 512],
            plain: [0; auth::PLAIN_BYTES],
            encoded: [0; auth::RESPONSE_BYTES],
        }
    }
    fn scratch(&mut self) -> Scratch<'_> {
        Scratch {
            input: &mut self.input,
            first_reply: &mut self.first,
            second_reply: &mut self.second,
            third_reply: &mut self.third,
            plain: &mut self.plain,
            encoded: &mut self.encoded,
        }
    }
}
fn drive(
    owner: Exchange<'_, Box<[u8; TLS_WIRE_BYTES]>>,
    peer: &mut Connection,
    script: &[(&[u8], &[u8])],
    chunk: usize,
    eof: bool,
) -> Result<Ready, Failed> {
    let mut owner = Some(owner);
    let mut round = 0;
    let mut received = Vec::new();
    let mut sent = 0;
    let mut closed = false;
    for _ in 0..100_000 {
        if !closed {
            if let Some((expected, response)) = script.get(round) {
                if received.len() < expected.len() {
                    let mut buffer = [0; LINE_BYTES];
                    match peer.read(&mut buffer).unwrap() {
                        IoProgress::Bytes(n) => received.extend_from_slice(&buffer[..n]),
                        IoProgress::Pending => {}
                        IoProgress::Closed => {}
                    }
                    assert!(expected.starts_with(&received), "unexpected AUTH bytes");
                }
                if received.len() == expected.len() {
                    if sent < response.len() {
                        let end = response.len().min(sent + chunk);
                        match peer.write(&response[sent..end]).unwrap() {
                            IoProgress::Bytes(n) => sent += n,
                            IoProgress::Pending => {}
                            IoProgress::Closed => panic!("closed peer"),
                        }
                    }
                    if peer.flush().unwrap() == FlushProgress::Complete && sent == response.len() {
                        round += 1;
                        received.clear();
                        sent = 0;
                        if round == script.len() && eof {
                            peer.abort();
                            closed = true;
                        }
                    }
                }
            }
        }
        match owner.take().unwrap().advance() {
            Ok(Progress::Pending(next)) => owner = Some(next),
            Ok(Progress::Ready(session)) => {
                assert_eq!(round, script.len());
                return Ok(session);
            }
            Err(error) => return Err(error),
        }
        std::thread::yield_now();
    }
    panic!("AUTH did not finish");
}
const PLAIN: &[u8] = b"AHByaXZhdGUtcmVsYXkAcGFzc3dvcmQ=\r\n";
const USER: &[u8] = b"cHJpdmF0ZS1yZWxheQ==\r\n";
const PASSWORD: &[u8] = b"cGFzc3dvcmQ=\r\n";

#[test]
fn relay_auth_plain_and_login_require_complete_positive_replies_on_real_tls() {
    let _serial = serial();
    let material = Material::new();
    for login in [false, true] {
        for chunk in [1, 512] {
            let advertisement = if login {
                b"250-local\r\n250-8BITMIME\r\n250 AUTH LOGIN\r\n".as_slice()
            } else {
                b"250-local\r\n250-8BITMIME\r\n250 AUTH PLAIN LOGIN\r\n".as_slice()
            };
            let (session, mut peer) = ready(&material, advertisement);
            let policy = session.policy_id();
            let mut cells = Cells::new();
            let mut meter = meter();
            let owner = Exchange::new(
                session,
                "private-relay",
                "password",
                cells.scratch(),
                material.source.clone(),
                cap(),
                &mut meter,
            )
            .ok()
            .unwrap();
            let plain = [
                (b"AUTH PLAIN\r\n".as_slice(), b"334 \r\n".as_slice()),
                (PLAIN, b"235-first\r\n235 authenticated\r\n".as_slice()),
            ];
            let login_script = [
                (
                    b"AUTH LOGIN\r\n".as_slice(),
                    b"334 VXNlcm5hbWU6\r\n".as_slice(),
                ),
                (USER, b"334 UGFzc3dvcmQ6\r\n".as_slice()),
                (PASSWORD, b"235 authenticated\r\n".as_slice()),
            ];
            let authenticated = drive(
                owner,
                &mut peer,
                if login { &login_script } else { &plain },
                chunk,
                false,
            )
            .ok()
            .unwrap();
            assert_eq!(authenticated.policy_id(), policy);
            assert!(authenticated.capabilities().eight_bit_mime());
            assert_eq!(
                authenticated.mechanism(),
                if login {
                    auth::Mechanism::Login
                } else {
                    auth::Mechanism::Plain
                }
            );
            drop(authenticated.into_buffers().unwrap());
            drop(peer.into_buffers().unwrap());
            assert!(meter.remaining().records < 100_000);
        }
    }
}

#[test]
fn relay_auth_refuses_bad_challenges_early_success_and_negative_finals() {
    let _serial = serial();
    let material = Material::new();
    for (login, challenge) in [
        (false, b"235 early\r\n".as_slice()),
        (false, b"535 denied\r\n"),
        (false, b"334 nonempty\r\n"),
        (false, b"334  \r\n"),
        (false, b"334-first\r\n334 \r\n"),
        (false, b"334 \n"),
        (false, b"334 \r\n235 injected\r\n"),
        (true, b"334 Zg=\r\n"),
        (true, b"334 Zh==\r\n"),
        (true, b"334 Zg==extra\r\n"),
        (true, b"334 Z g==\r\n"),
        (true, b"334 Zg==\t\r\n"),
        (true, b"334-\r\n334 \r\n"),
        (true, b"235 early\r\n"),
        (true, b"535 denied\r\n"),
    ] {
        let advertisement = if login {
            b"250-local\r\n250 AUTH LOGIN\r\n".as_slice()
        } else {
            b"250-local\r\n250 AUTH PLAIN LOGIN\r\n".as_slice()
        };
        let (session, mut peer) = ready(&material, advertisement);
        let mut cells = Cells::new();
        let mut meter = meter();
        let owner = Exchange::new(
            session,
            "private-relay",
            "password",
            cells.scratch(),
            material.source.clone(),
            cap(),
            &mut meter,
        )
        .ok()
        .unwrap();
        let command = if login {
            b"AUTH LOGIN\r\n".as_slice()
        } else {
            b"AUTH PLAIN\r\n".as_slice()
        };
        let failure = drive(owner, &mut peer, &[(command, challenge)], 512, false)
            .err()
            .unwrap();
        assert!(matches!(failure.error, Error::Invalid | Error::Forbidden));
        assert_eq!(
            failure.peer_rejection(),
            (challenge == b"535 denied\r\n").then_some(535)
        );
        drop(failure.into_buffers().unwrap());
        drop(peer.into_buffers().unwrap());
    }
    for response in [
        b"535 denied\r\n".as_slice(),
        b"535-denied\r\n535 details\r\n",
        b"535 denied\r\n250 injected\r\n",
        b"334 again\r\n",
        b"235-first\r\n535 mixed\r\n",
        b"235 ok\r\n250 injected\r\n",
        b"235 partial",
    ] {
        let (session, mut peer) = ready(&material, b"250-local\r\n250 AUTH PLAIN LOGIN\r\n");
        let mut cells = Cells::new();
        let mut meter = meter();
        let owner = Exchange::new(
            session,
            "private-relay",
            "password",
            cells.scratch(),
            material.source.clone(),
            cap(),
            &mut meter,
        )
        .ok()
        .unwrap();
        let failure = drive(
            owner,
            &mut peer,
            &[(b"AUTH PLAIN\r\n", b"334 \r\n"), (PLAIN, response)],
            512,
            response == b"235 partial",
        )
        .err()
        .unwrap();
        assert_eq!(
            failure.peer_rejection(),
            (response == b"535 denied\r\n").then_some(535)
        );
        drop(failure.into_buffers().unwrap());
        drop(peer.into_buffers().unwrap());
    }
}

#[test]
fn relay_auth_constructor_rejects_wrong_credentials_unsupported_mechanisms_and_short_cells() {
    let _serial = serial();
    let material = Material::new();
    for case in 0..9 {
        let advertisement = if case == 8 {
            b"250-local\r\n250 AUTH SCRAM-SHA-256\r\n".as_slice()
        } else {
            b"250-local\r\n250 AUTH PLAIN\r\n".as_slice()
        };
        let (session, mut peer) = ready(&material, advertisement);
        let mut cells = Cells::new();
        let mut scratch = cells.scratch();
        let mut meter = meter();
        match case {
            0 => scratch.input = &mut scratch.input[..511],
            1 => scratch.first_reply = &mut scratch.first_reply[..511],
            2 => scratch.second_reply = &mut scratch.second_reply[..511],
            3 => scratch.third_reply = &mut scratch.third_reply[..511],
            4 => scratch.plain = &mut scratch.plain[..auth::PLAIN_BYTES - 1],
            5 => scratch.encoded = &mut scratch.encoded[..auth::RESPONSE_BYTES - 1],
            _ => {}
        }
        let username = if case == 6 {
            "other-relay"
        } else {
            "private-relay"
        };
        let password = if case == 7 {
            "wrong-password"
        } else {
            "password"
        };
        let failure = Exchange::new(
            session,
            username,
            password,
            scratch,
            material.source.clone(),
            cap(),
            &mut meter,
        )
        .err()
        .unwrap();
        assert_eq!(
            failure.error,
            if case < 6 {
                Error::Capacity
            } else {
                Error::Forbidden
            }
        );
        assert_eq!(failure.peer_rejection(), None);
        drop(failure.into_buffers().unwrap());
        if let Ok(IoProgress::Bytes(n)) = peer.read(&mut [0; 512]) {
            assert_eq!(n, 0);
        }
        drop(peer.into_buffers().unwrap());
    }
}

struct ExpiringClock(std::sync::atomic::AtomicUsize);
impl Clock for ExpiringClock {
    fn sample(&self) -> Result<Time, Error> {
        use std::sync::atomic::Ordering;
        let count = self.0.fetch_add(1, Ordering::Relaxed);
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(if count >= 2 { 1000 } else { 0 }),
        })
    }
}
#[test]
fn relay_auth_holds_original_budget_and_checks_deadline_after_transport() {
    let _serial = serial();
    let material = Material::new();
    for case in 0..3 {
        let (session, peer) = ready(&material, b"250-local\r\n250 AUTH PLAIN\r\n");
        let mut cells = Cells::new();
        let mut meter = Meter::new(
            cap(),
            Charge {
                io_bytes: if case == 0 { 0 } else { 10000 },
                output_bytes: 10000,
                records: if case == 1 { 0 } else { 100 },
                unlinks: 0,
            },
        );
        let clock: Arc<dyn Clock> = if case == 2 {
            Arc::new(ExpiringClock(std::sync::atomic::AtomicUsize::new(0)))
        } else {
            material.source.clone()
        };
        let owner = Exchange::new(
            session,
            "private-relay",
            "password",
            cells.scratch(),
            clock,
            cap(),
            &mut meter,
        )
        .ok()
        .unwrap();
        let failure = owner.advance().err().unwrap();
        assert_eq!(
            failure.error,
            if case == 2 {
                Error::Deadline
            } else {
                Error::Capacity
            }
        );
        drop(failure.into_buffers().unwrap());
        drop(peer.into_buffers().unwrap());
    }
    let (session, mut peer) = ready(&material, b"250-local\r\n250 AUTH PLAIN\r\n");
    let mut cells = Cells::new();
    let mut meter = Meter::new(
        cap(),
        Charge {
            io_bytes: 64 * 1024 * 1024,
            records: 100_000,
            ..Charge::default()
        },
    );
    let owner = Exchange::new(
        session,
        "private-relay",
        "password",
        cells.scratch(),
        material.source.clone(),
        cap(),
        &mut meter,
    )
    .ok()
    .unwrap();
    let failure = drive(
        owner,
        &mut peer,
        &[(b"AUTH PLAIN\r\n", b"334 \r\n")],
        512,
        false,
    )
    .err()
    .unwrap();
    assert_eq!(failure.error, Error::Capacity);
    drop(failure.into_buffers().unwrap());
    drop(peer.into_buffers().unwrap());
}

#[test]
fn relay_auth_maximum_credentials_use_unwrapped_bounded_responses() {
    let _serial = serial();
    let material = Material::new();
    let username = "u".repeat(254);
    let password = "p".repeat(1024);
    let plain = concat!(
        "AHV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1",
        "dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1",
        "dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1",
        "dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1",
        "dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1AHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHA=",
        "\r\n"
    )
    .as_bytes();
    let user = concat!(
        "dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1",
        "dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1",
        "dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1",
        "dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1",
        "dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXU=",
        "\r\n"
    )
    .as_bytes();
    let password_wire = concat!(
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw",
        "cHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcA==",
        "\r\n"
    )
    .as_bytes();
    assert_eq!(plain.len(), auth::RESPONSE_BYTES);
    for login in [false, true] {
        let advertisement = if login {
            b"250-local\r\n250 AUTH LOGIN\r\n".as_slice()
        } else {
            b"250-local\r\n250 AUTH PLAIN\r\n".as_slice()
        };
        let (session, mut peer) = ready_credentials(&material, advertisement, &username, &password);
        let mut cells = Cells::new();
        let mut meter = meter();
        let owner = Exchange::new(
            session,
            &username,
            &password,
            cells.scratch(),
            material.source.clone(),
            cap(),
            &mut meter,
        )
        .ok()
        .unwrap();
        let plain_script = [
            (b"AUTH PLAIN\r\n".as_slice(), b"334 \r\n".as_slice()),
            (plain, b"235 ok\r\n".as_slice()),
        ];
        let login_script = [
            (b"AUTH LOGIN\r\n".as_slice(), b"334 \r\n".as_slice()),
            (user, b"334 \r\n".as_slice()),
            (password_wire, b"235 ok\r\n".as_slice()),
        ];
        let result = drive(
            owner,
            &mut peer,
            if login { &login_script } else { &plain_script },
            7,
            false,
        )
        .ok()
        .unwrap();
        drop(result.into_buffers().unwrap());
        drop(peer.into_buffers().unwrap());
    }
}

pub(super) fn authenticated(material: &Material) -> (Ready, Connection) {
    authenticated_with_advertisement(material, b"250-local\r\n250 AUTH PLAIN\r\n")
}
pub(super) fn authenticated_with_advertisement(
    material: &Material,
    advertisement: &[u8],
) -> (Ready, Connection) {
    let (session, mut peer) = ready(material, advertisement);
    let capabilities = session.capabilities();
    let mut cells = Cells::new();
    let mut meter = meter();
    let owner = Exchange::new(
        session,
        "private-relay",
        "password",
        cells.scratch(),
        material.source.clone(),
        cap(),
        &mut meter,
    )
    .ok()
    .unwrap();
    let session = drive(
        owner,
        &mut peer,
        &[
            (b"AUTH PLAIN\r\n", b"334 \r\n"),
            (PLAIN, b"235 authenticated\r\n"),
        ],
        512,
        false,
    )
    .ok()
    .unwrap();
    assert_eq!(session.capabilities(), capabilities);
    (session, peer)
}

#[test]
fn relay_auth_initial_plain_requires_final_reply_without_another_challenge() {
    let _serial = serial();
    let material = Material::new();
    let command = [b"AUTH PLAIN ".as_slice(), PLAIN].concat();
    for response in [
        b"235 authenticated\r\n".as_slice(),
        b"535 denied\r\n",
        b"334 Ok\r\n",
        b"235 ok\r\n235 injected\r\n",
    ] {
        for chunk in [1, 512] {
            if response == b"235 ok\r\n235 injected\r\n" && chunk == 1 {
                continue;
            }
            let (session, mut peer) = ready(&material, b"250-local\r\n250 AUTH PLAIN LOGIN\r\n");
            let mut cells = Cells::new();
            let mut meter = meter();
            let owner = Exchange::new(
                session,
                "private-relay",
                "password",
                cells.scratch(),
                material.source.clone(),
                cap(),
                &mut meter,
            )
            .ok()
            .unwrap()
            .with_initial_response()
            .ok()
            .unwrap();
            let result = drive(owner, &mut peer, &[(&command, response)], chunk, false);
            match result {
                Ok(session) => {
                    assert_eq!(response, b"235 authenticated\r\n");
                    drop(session.into_buffers().unwrap());
                }
                Err(failure) => {
                    assert_ne!(response, b"235 authenticated\r\n");
                    assert_eq!(
                        failure.peer_rejection(),
                        (response == b"535 denied\r\n").then_some(535)
                    );
                    drop(failure.into_buffers().unwrap());
                }
            }
            drop(peer.into_buffers().unwrap());
        }
    }
}

#[test]
fn relay_auth_initial_response_line_bound_keeps_long_responses_challenged() {
    let _serial = serial();
    let material = Material::new();
    let username = "u".repeat(254);
    for (password_len, encoded, initial) in [
        (116, "AHV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1AHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBw\r\n", true),
        (117, "AHV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1dXV1AHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcA==\r\n", false),
    ] {
        let password = "p".repeat(password_len);
        let command = [b"AUTH PLAIN ".as_slice(), encoded.as_bytes()].concat();
        assert_eq!(command.len(), if initial { 509 } else { 513 });
        let (session, mut peer) = ready_credentials(&material, b"250-local\r\n250 AUTH PLAIN\r\n", &username, &password);
        let mut cells = Cells::new();
        let mut meter = meter();
        let owner = Exchange::new(session, &username, &password, cells.scratch(), material.source.clone(), cap(), &mut meter)
            .ok().unwrap().with_initial_response().ok().unwrap();
        let first = [(&command[..], b"235 ok\r\n".as_slice())];
        let challenged = [(b"AUTH PLAIN\r\n".as_slice(), b"334 \r\n".as_slice()), (encoded.as_bytes(), b"235 ok\r\n".as_slice())];
        let session = drive(owner, &mut peer, if initial { &first } else { &challenged }, 1, false).ok().unwrap();
        drop(session.into_buffers().unwrap());
        drop(peer.into_buffers().unwrap());
    }
}
