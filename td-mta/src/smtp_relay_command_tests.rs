use super::*;
use crate::{
    ports::{Deadline, FlushProgress, IoProgress, Transport},
    smtp_relay_command::{Command, Exchange, Progress, Refusal, Reply, Scratch},
    smtp_wire::LINE_BYTES,
};
use td_mime::work::{Charge, Meter};
type Connection = TlsConnection<Box<[u8; TLS_WIRE_BYTES]>>;
type Ready<'a> = Reply<'a, Box<[u8; TLS_WIRE_BYTES]>>;
type Failed<'a> = Refusal<'a, Box<[u8; TLS_WIRE_BYTES]>>;
fn cap() -> Deadline {
    Deadline::after(Tick(0), 1000).unwrap()
}
fn meter() -> Meter {
    Meter::new(
        cap(),
        Charge {
            io_bytes: 64 * 1024 * 1024,
            records: 100_000,
            output_bytes: 8192,
            unlinks: 0,
        },
    )
}
struct Cells {
    input: [u8; 512],
    line: [u8; 512],
    reply: [u8; 4096],
    command: [u8; 512],
}
impl Cells {
    fn new() -> Self {
        Self {
            input: [0; 512],
            line: [0; 512],
            reply: [0; 4096],
            command: [0; 512],
        }
    }
    fn scratch(&mut self) -> Scratch<'_> {
        Scratch {
            input: &mut self.input,
            line: &mut self.line,
            reply: &mut self.reply,
            command: &mut self.command,
        }
    }
}
fn drive<'a>(
    owner: Exchange<'a, Box<[u8; TLS_WIRE_BYTES]>>,
    peer: &mut Connection,
    expected: &[u8],
    response: &[u8],
    chunk: usize,
    eof: bool,
) -> Result<Ready<'a>, Failed<'a>> {
    let mut owner = Some(owner);
    let mut received = Vec::new();
    let mut sent = 0;
    let mut closed = false;
    for _ in 0..100_000 {
        if !closed {
            if received.len() < expected.len() {
                let mut buffer = [0; LINE_BYTES];
                if let IoProgress::Bytes(n) = peer.read(&mut buffer).unwrap() {
                    received.extend_from_slice(&buffer[..n]);
                }
                assert!(expected.starts_with(&received));
            }
            if received.len() == expected.len() {
                if sent < response.len() {
                    let end = response.len().min(sent + chunk);
                    if let IoProgress::Bytes(n) = peer.write(&response[sent..end]).unwrap() {
                        sent += n;
                    }
                }
                if peer.flush().unwrap() == FlushProgress::Complete && sent == response.len() && eof
                {
                    peer.abort();
                    closed = true;
                }
            }
        }
        match owner.take().unwrap().advance() {
            Ok(Progress::Pending(next)) => owner = Some(next),
            Ok(Progress::Ready(reply)) => {
                assert_eq!(received, expected);
                return Ok(reply);
            }
            Err(error) => return Err(error),
        }
        std::thread::yield_now();
    }
    panic!("SMTP command did not finish");
}
#[test]
fn relay_commands_preserve_envelope_octets_and_complete_normalized_replies() {
    let _serial = serial();
    let material = Material::new();
    for chunk in [1, 512] {
        let (mut session, mut peer) = relay_auth_tests::authenticated(&material);
        let policy = session.policy_id();
        for (command, expected, response, code, text) in [
            (
                Command::MailFrom(""),
                b"MAIL FROM:<>\r\n".as_slice(),
                b"250-local\r\n250 accepted \t\r\n".as_slice(),
                250,
                "250 local accepted \t",
            ),
            (
                Command::MailFrom("User@Example.test"),
                b"MAIL FROM:<User@Example.test>\r\n",
                b"250 ok\r\n",
                250,
                "250 ok",
            ),
            (
                Command::RcptTo("\"a> b\"@Example.test"),
                b"RCPT TO:<\"a> b\"@Example.test>\r\n",
                b"550 unknown\r\n",
                550,
                "550 unknown",
            ),
            (
                Command::Data,
                b"DATA\r\n",
                b"354 continue\r\n",
                354,
                "354 continue",
            ),
        ] {
            let mut cells = Cells::new();
            let mut meter = meter();
            let owner = Exchange::new(
                session,
                command,
                cells.scratch(),
                material.source.clone(),
                cap(),
                &mut meter,
            )
            .ok()
            .unwrap();
            let reply = drive(owner, &mut peer, expected, response, chunk, false)
                .ok()
                .unwrap();
            assert_eq!(reply.code().unwrap(), code);
            assert_eq!(reply.text().unwrap(), text);
            session = reply.into_session();
            assert_eq!(session.policy_id(), policy);
        }
        drop(session.into_buffers().unwrap());
        drop(peer.into_buffers().unwrap());
    }
}
#[test]
fn relay_commands_refuse_malformed_incomplete_tailed_and_oversized_replies() {
    let _serial = serial();
    let material = Material::new();
    let oversized = [b"250 ".as_slice(), &[b'x'; 512], b"\r\n"].concat();
    let mut too_much = b"250-a\r\n".to_vec();
    for _ in 0..9 {
        too_much.extend_from_slice(b"250-");
        too_much.extend_from_slice(&[b'b'; 500]);
        too_much.extend_from_slice(b"\r\n");
    }
    too_much.extend_from_slice(b"250 done\r\n");
    for (response, eof) in [
        (b"250 partial".as_slice(), true),
        (b"250 bare\n", false),
        (b"250-first\r\n550 mismatch\r\n", false),
        (b"250 ok\r\n250 injected\r\n", false),
        (&oversized, false),
        (&too_much, false),
    ] {
        let (session, mut peer) = relay_auth_tests::authenticated(&material);
        let mut cells = Cells::new();
        let mut meter = meter();
        let owner = Exchange::new(
            session,
            Command::MailFrom(""),
            cells.scratch(),
            material.source.clone(),
            cap(),
            &mut meter,
        )
        .ok()
        .unwrap();
        let failure = drive(owner, &mut peer, b"MAIL FROM:<>\r\n", response, 512, eof)
            .err()
            .unwrap();
        drop(failure.into_buffers().unwrap());
        drop(peer.into_buffers().unwrap());
    }
}
#[test]
fn relay_commands_reject_injected_paths_and_short_reservations_before_writing() {
    let _serial = serial();
    let material = Material::new();
    for command in [
        Command::RcptTo(""),
        Command::RcptTo("a@example.test\r\nDATA"),
        Command::MailFrom("<>"),
        Command::MailFrom("a@example.test SIZE=1"),
        Command::RcptTo("a@[127.0.0.1]"),
        Command::MailFrom("é@example.test"),
    ] {
        let (session, mut peer) = relay_auth_tests::authenticated(&material);
        let mut cells = Cells::new();
        let mut meter = meter();
        let failure = Exchange::new(
            session,
            command,
            cells.scratch(),
            material.source.clone(),
            cap(),
            &mut meter,
        )
        .err()
        .unwrap();
        assert_eq!(failure.error, Error::Invalid);
        drop(failure.into_buffers().unwrap());
        if let Ok(IoProgress::Bytes(n)) = peer.read(&mut [0; 512]) {
            assert_eq!(n, 0);
        }
        drop(peer.into_buffers().unwrap());
    }
    for short in 0..4 {
        let (session, peer) = relay_auth_tests::authenticated(&material);
        let mut cells = Cells::new();
        let mut scratch = cells.scratch();
        let mut meter = meter();
        match short {
            0 => scratch.input = &mut scratch.input[..511],
            1 => scratch.line = &mut scratch.line[..511],
            2 => scratch.reply = &mut scratch.reply[..4095],
            _ => scratch.command = &mut scratch.command[..511],
        }
        let failure = Exchange::new(
            session,
            Command::Data,
            scratch,
            material.source.clone(),
            cap(),
            &mut meter,
        )
        .err()
        .unwrap();
        assert_eq!(failure.error, Error::Capacity);
        drop(failure.into_buffers().unwrap());
        drop(peer.into_buffers().unwrap());
    }
}

struct ExpiringClock(std::sync::atomic::AtomicUsize);
impl Clock for ExpiringClock {
    fn sample(&self) -> Result<Time, Error> {
        let count = self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(if count >= 3 { 1000 } else { 0 }),
        })
    }
}
#[test]
fn relay_command_budget_and_post_transport_deadline_are_not_refreshed() {
    let _serial = serial();
    let material = Material::new();
    for expire in [false, true] {
        let (session, peer) = relay_auth_tests::authenticated(&material);
        let mut cells = Cells::new();
        let mut meter = Meter::new(
            cap(),
            Charge {
                io_bytes: if expire { 1000 } else { 0 },
                records: 100,
                ..Charge::default()
            },
        );
        let clock: Arc<dyn Clock> = if expire {
            Arc::new(ExpiringClock(std::sync::atomic::AtomicUsize::new(0)))
        } else {
            material.source.clone()
        };
        let owner = Exchange::new(
            session,
            Command::Data,
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
            if expire {
                Error::Deadline
            } else {
                Error::Capacity
            }
        );
        drop(failure.into_buffers().unwrap());
        drop(peer.into_buffers().unwrap());
    }
}

#[test]
fn eight_bit_mail_requires_retained_tls_advertisement_before_writing() {
    let _serial = serial();
    let material = Material::new();
    for (advertisement, offered) in [
        (
            b"250-local\r\n250-8BITMIME\r\n250 AUTH PLAIN\r\n".as_slice(),
            true,
        ),
        (b"250-local\r\n250 AUTH PLAIN\r\n", false),
        (b"250-8BITMIME\r\n250 AUTH PLAIN\r\n", false),
        (
            b"250-local\r\n250-8BITMIME bogus\r\n250 AUTH PLAIN\r\n",
            false,
        ),
    ] {
        for address in ["", "User@Example.test", "\"a> b\"@Example.test"] {
            let (session, mut peer) =
                relay_auth_tests::authenticated_with_advertisement(&material, advertisement);
            assert_eq!(session.capabilities().eight_bit_mime(), offered);
            let mut cells = Cells::new();
            let mut meter = meter();
            let result = Exchange::new(
                session,
                Command::MailFromEightBit(address),
                cells.scratch(),
                material.source.clone(),
                cap(),
                &mut meter,
            );
            if offered {
                let expected = format!("MAIL FROM:<{address}> BODY=8BITMIME\r\n");
                let reply = drive(
                    result.ok().unwrap(),
                    &mut peer,
                    expected.as_bytes(),
                    b"250 accepted\r\n",
                    1,
                    false,
                )
                .ok()
                .unwrap();
                assert_eq!(reply.code().unwrap(), 250);
                assert!(reply.into_session().capabilities().eight_bit_mime());
            } else {
                let failure = result.err().unwrap();
                assert_eq!(failure.error, Error::Forbidden);
                drop(failure.into_buffers().unwrap());
                if let Ok(IoProgress::Bytes(n)) = peer.read(&mut [0; 512]) {
                    assert_eq!(n, 0);
                }
            }
            drop(peer.into_buffers().unwrap());
        }
    }
    for address in [
        "a@example.test\r\nDATA",
        "a@example.test SIZE=1",
        "é@example.test",
    ] {
        let (session, mut peer) = relay_auth_tests::authenticated_with_advertisement(
            &material,
            b"250-local\r\n250-8BITMIME\r\n250 AUTH PLAIN\r\n",
        );
        let mut cells = Cells::new();
        let mut meter = meter();
        let failure = Exchange::new(
            session,
            Command::MailFromEightBit(address),
            cells.scratch(),
            material.source.clone(),
            cap(),
            &mut meter,
        )
        .err()
        .unwrap();
        assert_eq!(failure.error, Error::Invalid);
        drop(failure.into_buffers().unwrap());
        if let Ok(IoProgress::Bytes(n)) = peer.read(&mut [0; 512]) {
            assert_eq!(n, 0);
        }
        drop(peer.into_buffers().unwrap());
    }
}

#[derive(Default)]
struct PostReadClock(std::sync::atomic::AtomicUsize);
impl Clock for PostReadClock {
    fn sample(&self) -> Result<Time, Error> {
        let left = self.0.load(std::sync::atomic::Ordering::Relaxed);
        if left > 1 {
            self.0.store(left - 1, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(if left == 1 { 1000 } else { 0 }),
        })
    }
}
#[test]
fn complete_reply_survives_post_read_expiry_without_session() {
    let _serial = serial();
    let material = Material::new();
    for (response, expected) in [
        (b"250 queued\r\n".as_slice(), Some((250, "250 queued"))),
        (b"550 refused\r\n".as_slice(), Some((550, "550 refused"))),
        (b"354 continue\r\n".as_slice(), Some((354, "354 continue"))),
        (b"250 partial".as_slice(), None),
        (b"250 queued\r\nextra".as_slice(), None),
    ] {
        let (session, mut peer) = relay_auth_tests::authenticated(&material);
        let clock = Arc::new(PostReadClock::default());
        let mut cells = Cells::new();
        let mut work = meter();
        let mut exchange = Exchange::new(
            session,
            Command::Data,
            cells.scratch(),
            clock.clone(),
            cap(),
            &mut work,
        )
        .ok()
        .unwrap();
        for _ in 0..100_000 {
            if !exchange.writes_pending() {
                break;
            }
            exchange = match exchange.advance().ok().unwrap() {
                Progress::Pending(next) => next,
                Progress::Ready(_) => panic!("reply before peer sent one"),
            };
        }
        assert!(!exchange.writes_pending());
        let mut received = Vec::new();
        for _ in 0..100_000 {
            let mut bytes = [0; 512];
            if let IoProgress::Bytes(n) = peer.read(&mut bytes).unwrap() {
                received.extend_from_slice(&bytes[..n]);
            }
            if received == b"DATA\r\n" {
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(received, b"DATA\r\n");
        let mut sent = 0;
        let mut flushed = false;
        for _ in 0..100_000 {
            if sent < response.len() {
                if let IoProgress::Bytes(n) = peer.write(&response[sent..]).unwrap() {
                    sent += n;
                }
            }
            if peer.flush().unwrap() == FlushProgress::Complete && sent == response.len() {
                flushed = true;
                break;
            }
            std::thread::yield_now();
        }
        assert!(flushed);
        // TlsIo receives the five-byte record header before its payload.
        exchange = match exchange.advance().ok().unwrap() {
            Progress::Pending(next) => next,
            Progress::Ready(_) => panic!("plaintext arrived before record payload"),
        };
        clock.0.store(2, std::sync::atomic::Ordering::Relaxed);
        let failure = exchange.advance().err().unwrap();
        assert_eq!(failure.reply(), expected);
        assert_eq!(
            failure.error,
            if response.ends_with(b"extra") {
                Error::Invalid
            } else {
                Error::Deadline
            }
        );
        drop(failure.into_buffers().unwrap());
        drop(peer.into_buffers().unwrap());
    }
}

#[test]
fn productive_output_and_partial_reply_reset_pacing_hint_on_idle() {
    let _serial = serial();
    let material = Material::new();
    let (session, mut peer) = relay_auth_tests::authenticated(&material);
    let mut cells = Cells::new();
    let mut work = meter();
    let mut owner = Exchange::new(
        session,
        Command::RcptTo("test@example.test"),
        cells.scratch(),
        material.source.clone(),
        cap(),
        &mut work,
    )
    .ok()
    .unwrap();
    assert!(!owner.transport_progress());
    let mut output_turns = 0;
    for _ in 0..100_000 {
        if !owner.writes_pending() {
            break;
        }
        owner = match owner.advance() {
            Ok(Progress::Pending(next)) => next,
            _ => panic!("command output must retain reply custody"),
        };
        output_turns += usize::from(owner.transport_progress());
        std::thread::yield_now();
    }
    assert!(!owner.writes_pending());
    assert!(output_turns >= 2);
    owner = match owner.advance() {
        Ok(Progress::Pending(next)) => next,
        _ => panic!("absent reply must remain pending"),
    };
    assert!(!owner.transport_progress());
    let command = b"RCPT TO:<test@example.test>\r\n";
    let partial = b"250-first\r\n";
    let mut received = Vec::new();
    let mut sent = 0;
    let mut productive = false;
    for _ in 0..100_000 {
        if received.len() < command.len() {
            let mut input = [0; LINE_BYTES];
            if let IoProgress::Bytes(n) = peer.read(&mut input).unwrap() {
                received.extend_from_slice(&input[..n]);
            }
            assert!(command.starts_with(&received));
        }
        if received.len() == command.len() {
            if sent < partial.len() {
                if let IoProgress::Bytes(n) = peer.write(&partial[sent..]).unwrap() {
                    sent += n;
                }
            }
            peer.flush().unwrap();
        }
        owner = match owner.advance() {
            Ok(Progress::Pending(next)) => next,
            _ => panic!("incomplete reply must retain command custody"),
        };
        if owner.transport_progress() {
            assert_eq!(sent, partial.len());
            productive = true;
            break;
        }
        std::thread::yield_now();
    }
    assert!(productive);
    owner = match owner.advance() {
        Ok(Progress::Pending(next)) => next,
        _ => panic!("idle partial reply must remain pending"),
    };
    assert!(!owner.transport_progress());
    let reply = drive(owner, &mut peer, b"", b"250 done\r\n", 512, false)
        .ok()
        .unwrap();
    assert_eq!(reply.text().unwrap(), "250 first done");
    drop(reply.into_session().into_buffers().unwrap());
    drop(peer.into_buffers().unwrap());
}
