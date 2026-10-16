use super::*;
use crate::{
    ports::{Deadline, FlushProgress, IoProgress, Transport},
    smtp_relay_ehlo::{Exchange, Progress, Refusal, Scratch, Session},
    smtp_wire::LINE_BYTES,
};

type Connection = TlsConnection<Box<[u8; TLS_WIRE_BYTES]>>;
type Ready = Session<Box<[u8; TLS_WIRE_BYTES]>>;
type Failed = Refusal<Box<[u8; TLS_WIRE_BYTES]>>;

fn cap(tick: u64) -> Deadline {
    Deadline::after(Tick(0), tick).unwrap()
}
pub(super) fn send(peer: &mut Connection, bytes: &[u8]) {
    let mut written = 0;
    for _ in 0..100_000 {
        if written < bytes.len() {
            match peer.write(&bytes[written..]).unwrap() {
                IoProgress::Bytes(n) => written += n,
                IoProgress::Pending => {}
                IoProgress::Closed => panic!("closed fixture peer"),
            }
        }
        if peer.flush().unwrap() == FlushProgress::Complete && written == bytes.len() {
            return;
        }
        std::thread::yield_now();
    }
    panic!("fixture send did not finish");
}
pub(super) fn drive(
    owner: Exchange<'_, Box<[u8; TLS_WIRE_BYTES]>>,
    peer: &mut Connection,
    response: &[u8],
    chunk: usize,
    eof: bool,
) -> Result<Ready, Failed> {
    let expected = b"EHLO mx.example.test\r\n";
    let mut request = Vec::new();
    let mut sent = 0;
    let mut closed = false;
    let mut owner = Some(owner);
    for _ in 0..100_000 {
        if !closed {
            if request.len() < expected.len() {
                let mut output = [0; LINE_BYTES];
                match peer.read(&mut output).unwrap() {
                    IoProgress::Bytes(n) => request.extend_from_slice(&output[..n]),
                    IoProgress::Pending => {}
                    IoProgress::Closed => {}
                }
                assert!(expected.starts_with(&request));
            }
            if request.len() == expected.len() {
                if sent < response.len() {
                    let end = response.len().min(sent + chunk);
                    match peer.write(&response[sent..end]).unwrap() {
                        IoProgress::Bytes(n) => sent += n,
                        IoProgress::Pending => {}
                        IoProgress::Closed => panic!("closed fixture peer"),
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
            Ok(Progress::Ready(ready)) => {
                assert_eq!(request, expected);
                return Ok(ready);
            }
            Err(error) => return Err(error),
        }
        std::thread::yield_now();
    }
    panic!("relay EHLO did not finish");
}

#[test]
fn relay_ehlo_retains_fragmented_capabilities_on_verified_tls_in_both_modes() {
    let _serial = serial();
    let material = Material::new();
    for starttls in [false, true] {
        let source = if starttls {
            source(false).replace(
                "port = 465\n",
                "port = 587\ntransport = \"required_starttls\"\n",
            )
        } else {
            source(false)
        };
        let lease = material.published(&source);
        let pool = HandshakePool::new(2).unwrap();
        let (mut client, mut peer) = io_tests::pair(&material, &lease, &pool, "smtp");
        io_tests::complete(&mut client, &mut peer).unwrap();
        if !starttls {
            send(&mut peer, b"220-first line\r\n220 ready\r\n");
        }
        let (mut input, mut greeting, mut reply, mut command) = (
            [0; LINE_BYTES],
            [0; LINE_BYTES],
            [0; LINE_BYTES],
            [0; LINE_BYTES],
        );
        let owner = Exchange::new(
            client,
            "mx.example.test",
            Scratch {
                input: &mut input,
                greeting: &mut greeting,
                reply: &mut reply,
                command: &mut command,
            },
            material.source.clone(),
            cap(1000),
        )
        .ok()
        .unwrap();
        let ready = drive(
            owner,
            &mut peer,
            b"250-localhost\r\n250-AUTH PLAIN LOGIN\r\n250-8BITMIME\r\n250 SIZE 1000\r\n",
            1,
            false,
        )
        .ok()
        .unwrap();
        assert_eq!(ready.check_credentials("private-relay", "password"), Ok(()));
        assert_eq!(
            ready.check_credentials("other-relay", "password"),
            Err(Error::Forbidden)
        );
        assert!(ready.mechanisms().plain());
        assert!(ready.mechanisms().login());
        assert!(ready.capabilities().eight_bit_mime());
        assert_eq!(ready.policy_id(), TlsPolicies::relay(&lease).unwrap());
        drop(ready.into_buffers().unwrap());
        drop(peer.into_buffers().unwrap());
    }
}

#[test]
fn relay_ehlo_refuses_invalid_greetings_and_capability_replies() {
    let _serial = serial();
    let material = Material::new();
    let lease = material.published(&source(false));
    let pool = HandshakePool::new(2).unwrap();
    let oversized = [b"250 ".as_slice(), &[b'X'; LINE_BYTES], b"\r\n"].concat();
    for (greeting_wire, response, eof) in [
        (b"550 unavailable\r\n".as_slice(), b"".as_slice(), false),
        (b"220-first\r\n250 wrong\r\n", b"", false),
        (b"220 ready\r\n250 injected\r\n", b"", false),
        (b"220 ready\r\n", b"550 no EHLO\r\n", false),
        (b"220 ready\r\n", b"250-host\r\n550 wrong\r\n", false),
        (b"220 ready\r\n", b"250-host\r\n250 AUTH PLAIN\n", false),
        (
            b"220 ready\r\n",
            b"250-host\r\n250 AUTH PLAIN\r\n235 injected\r\n",
            false,
        ),
        (b"220 ready\r\n", b"250-host\r\n250-AUTH PLAIN\r\n", true),
        (b"220 ready\r\n", oversized.as_slice(), false),
    ] {
        let (mut client, mut peer) = io_tests::pair(&material, &lease, &pool, "smtp");
        io_tests::complete(&mut client, &mut peer).unwrap();
        send(&mut peer, greeting_wire);
        let (mut input, mut greeting, mut reply, mut command) = (
            [0; LINE_BYTES],
            [0; LINE_BYTES],
            [0; LINE_BYTES],
            [0; LINE_BYTES],
        );
        let owner = Exchange::new(
            client,
            "mx.example.test",
            Scratch {
                input: &mut input,
                greeting: &mut greeting,
                reply: &mut reply,
                command: &mut command,
            },
            material.source.clone(),
            cap(1000),
        )
        .ok()
        .unwrap();
        let refused = drive(owner, &mut peer, response, response.len().max(1), eof)
            .err()
            .unwrap();
        drop(refused.into_buffers().unwrap());
        drop(peer.into_buffers().unwrap());
    }
}

#[test]
fn relay_ehlo_admission_refuses_unverified_role_bad_hostname_deadline_and_short_cells() {
    let _serial = serial();
    let material = Material::new();
    let lease = material.published(&source(false));
    let pool = HandshakePool::new(2).unwrap();
    for mode in 0..8 {
        let (mut client, mut peer) = io_tests::pair(&material, &lease, &pool, "smtp");
        if mode != 0 {
            io_tests::complete(&mut client, &mut peer).unwrap();
        }
        if mode == 1 {
            std::mem::swap(&mut client, &mut peer);
        }
        let (mut input, mut greeting, mut reply, mut command) = (
            [0; LINE_BYTES],
            [0; LINE_BYTES],
            [0; LINE_BYTES],
            [0; LINE_BYTES],
        );
        let result = Exchange::new(
            client,
            if mode == 2 {
                "mx\r\nAUTH PLAIN"
            } else {
                "mx.example.test"
            },
            Scratch {
                input: if mode == 4 {
                    &mut input[..511]
                } else {
                    &mut input
                },
                greeting: if mode == 5 {
                    &mut greeting[..511]
                } else {
                    &mut greeting
                },
                reply: if mode == 6 {
                    &mut reply[..511]
                } else {
                    &mut reply
                },
                command: if mode == 7 {
                    &mut command[..511]
                } else {
                    &mut command
                },
            },
            material.source.clone(),
            cap(if mode == 3 { 0 } else { 1000 }),
        );
        let refused = result.err().unwrap();
        assert_eq!(
            refused.error,
            match mode {
                0 | 1 => Error::Tls,
                2 => Error::Invalid,
                3 => Error::Deadline,
                _ => Error::Capacity,
            }
        );
        drop(refused.into_buffers().unwrap());
        drop(peer.into_buffers().unwrap());
    }
}

struct ExpiringClock(std::sync::atomic::AtomicUsize);
impl Clock for ExpiringClock {
    fn sample(&self) -> Result<Time, Error> {
        let call = self.0.fetch_add(1, Ordering::Relaxed);
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(if call >= 2 { 1000 } else { 0 }),
        })
    }
}
#[test]
fn relay_ehlo_rechecks_deadline_after_transport_progress() {
    let _serial = serial();
    let material = Material::new();
    let lease = material.published(&source(false));
    let pool = HandshakePool::new(2).unwrap();
    let (mut client, mut peer) = io_tests::pair(&material, &lease, &pool, "smtp");
    io_tests::complete(&mut client, &mut peer).unwrap();
    send(&mut peer, b"220 ready\r\n");
    let (mut input, mut greeting, mut reply, mut command) = (
        [0; LINE_BYTES],
        [0; LINE_BYTES],
        [0; LINE_BYTES],
        [0; LINE_BYTES],
    );
    let clock = Arc::new(ExpiringClock(std::sync::atomic::AtomicUsize::new(0)));
    let owner = Exchange::new(
        client,
        "mx.example.test",
        Scratch {
            input: &mut input,
            greeting: &mut greeting,
            reply: &mut reply,
            command: &mut command,
        },
        clock.clone(),
        cap(1000),
    )
    .ok()
    .unwrap();
    let refused = owner.advance().err().unwrap();
    assert_eq!(refused.error, Error::Deadline);
    assert_eq!(clock.0.load(Ordering::Relaxed), 3);
    drop(refused.into_buffers().unwrap());
    drop(peer.into_buffers().unwrap());
}

#[test]
fn relay_ehlo_refuses_verified_acme_role() {
    let _serial = serial();
    let material = Material::new();
    let lease = material.published(&client_tests::acme_source());
    let pool = HandshakePool::new(2).unwrap();
    let (mut client, mut peer) = io_tests::pair_for_policy(
        &material,
        &lease,
        &pool,
        TlsPolicies::acme(&lease).unwrap(),
        "smtp",
    );
    io_tests::complete(&mut client, &mut peer).unwrap();
    let (mut input, mut greeting, mut reply, mut command) = (
        [0; LINE_BYTES],
        [0; LINE_BYTES],
        [0; LINE_BYTES],
        [0; LINE_BYTES],
    );
    let refused = Exchange::new(
        client,
        "mx.example.test",
        Scratch {
            input: &mut input,
            greeting: &mut greeting,
            reply: &mut reply,
            command: &mut command,
        },
        material.source.clone(),
        cap(1000),
    )
    .err()
    .unwrap();
    assert_eq!(refused.error, Error::Forbidden);
    drop(refused.into_buffers().unwrap());
    drop(peer.into_buffers().unwrap());
}
