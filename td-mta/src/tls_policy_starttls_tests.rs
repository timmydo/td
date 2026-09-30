use super::*;
use crate::{
    ports::{
        Deadline, FlushProgress, Handshake, IoProgress, PeerVerification, TlsTransport, Transport,
    },
    smtp_wire::{LineReader, ReplyReader, LINE_BYTES},
    transport::TcpTransport,
};
use std::{
    net::{TcpListener, TcpStream},
    sync::atomic::AtomicU64,
};

fn sockets() -> (TcpTransport, TcpTransport) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    (
        TcpTransport::from_stream(client).unwrap(),
        TcpTransport::from_stream(server).unwrap(),
    )
}
fn cap(tick: u64) -> Deadline {
    Deadline::after(Tick(0), tick).unwrap()
}
fn held(
    lease: &GenerationLease<TlsPolicies>,
    id: TlsPolicyId,
    pool: &HandshakePool,
) -> SessionReservation<Box<[u8; TLS_WIRE_BYTES]>> {
    let (input, output) = buffers();
    TlsPolicies::reserve_session(lease.clone(), id, pool, input, output)
        .unwrap()
        .construct()
        .unwrap()
}

#[test]
fn server_starttls_flushes_220_before_handoff_and_verifies_real_tls() {
    let _serial = serial();
    let material = Material::new();
    let lease = material.published(&source(false));
    let pool = HandshakePool::new(2).unwrap();
    let (mut client, mut server) = sockets();
    assert_eq!(client.write(b"sTaRtTlS\r\n"), Ok(IoProgress::Bytes(10)));
    assert_eq!(client.flush(), Ok(FlushProgress::Complete));
    let mut scratch = [0; LINE_BYTES];
    let mut reader = LineReader::new(&mut scratch).unwrap();
    let mut input = [0; LINE_BYTES];
    let mut complete = false;
    for _ in 0..100_000 {
        match server.read(&mut input).unwrap() {
            IoProgress::Bytes(n) => {
                let progress = reader.feed(&input[..n]).unwrap();
                assert_eq!(progress.consumed, n);
                if progress.complete {
                    complete = true;
                    break;
                }
            }
            IoProgress::Pending => std::thread::yield_now(),
            IoProgress::Closed => panic!("client closed before STARTTLS"),
        }
    }
    assert!(complete);
    let id = TlsPolicies::listener(&lease, "smtp").unwrap();
    let owner = ServerStartTls::new(
        held(&lease, id, &pool),
        server,
        &reader,
        &[],
        material.source.clone(),
        cap(1000),
        cap(100),
    )
    .ok()
    .unwrap();
    assert_eq!(pool.available(), 1);
    let mut owner = Some(owner);
    let mut result = None;
    for _ in 0..100_000 {
        match owner.take().unwrap().advance().ok().unwrap() {
            ServerUpgradeProgress::Pending(next) => owner = Some(next),
            ServerUpgradeProgress::Tls(connection) => {
                result = Some(connection);
                break;
            }
        }
    }
    let mut server = result.unwrap();
    assert_eq!(server.info(), None);
    assert_eq!(pool.available(), 1);
    let mut scratch = [0; LINE_BYTES];
    let mut reply = ReplyReader::new(&mut scratch).unwrap();
    for _ in 0..100_000 {
        match client.read(&mut input).unwrap() {
            IoProgress::Bytes(n) => {
                let progress = reply.feed(&input[..n]).unwrap();
                assert_eq!(progress.consumed, n);
                if reply.complete() {
                    break;
                }
            }
            IoProgress::Pending => std::thread::yield_now(),
            IoProgress::Closed => panic!("server closed before 220"),
        }
    }
    assert!(reply.complete());
    assert_eq!(reply.line().unwrap().code, 220);
    let mut client = held(&lease, TlsPolicies::relay(&lease).unwrap(), &pool)
        .handoff(client, &[], material.source.clone(), cap(1000), cap(100))
        .unwrap();
    let mut completed = false;
    for _ in 0..100_000 {
        if let (Handshake::Complete(c), Handshake::Complete(s)) = (
            client.handshake(cap(100)).unwrap(),
            server.handshake(cap(100)).unwrap(),
        ) {
            assert_eq!(c.peer, PeerVerification::ServerName);
            assert_eq!(s.peer, PeerVerification::None);
            completed = true;
            break;
        }
        std::thread::yield_now();
    }
    assert!(completed);
    assert_eq!(pool.available(), 2);
    drop(client.into_buffers().unwrap());
    drop(server.into_buffers().unwrap());
}

#[test]
fn server_starttls_refuses_unframed_parameterized_tailed_and_wrong_role_commands() {
    let _serial = serial();
    let material = Material::new();
    let lease = material.published(&source(false));
    let pool = HandshakePool::new(1).unwrap();
    for (command, role) in [
        (b"STARTTLS\r".as_slice(), "smtp"),
        (b"STARTTLS arg\r\n", "smtp"),
        (b"STARTTLS \r\n", "smtp"),
        (b"STARTTLS\r\nNOOP\r\n", "smtp"),
        (b"STARTTLS\r\n\x16\x03\x03", "smtp"),
        (b"STARTTLS\r\n", "https"),
        (b"STARTTLS\r\n", "relay"),
    ] {
        let (mut peer, plain) = sockets();
        let mut scratch = [0; LINE_BYTES];
        let mut reader = LineReader::new(&mut scratch).unwrap();
        let progress = reader.feed(command).unwrap();
        let (input, output) = buffers();
        let pointers = (input.as_ptr(), output.as_ptr());
        let reserved = TlsPolicies::reserve_session(
            lease.clone(),
            if role == "relay" {
                TlsPolicies::relay(&lease).unwrap()
            } else {
                TlsPolicies::listener(&lease, role).unwrap()
            },
            &pool,
            input,
            output,
        )
        .unwrap()
        .construct()
        .unwrap();
        let refusal = ServerStartTls::new(
            reserved,
            plain,
            &reader,
            &command[progress.consumed..],
            material.source.clone(),
            cap(1000),
            cap(100),
        )
        .err()
        .unwrap();
        assert_eq!(refusal.error(), Error::Invalid);
        let (input, output) = refusal.into_buffers();
        assert_eq!((input.as_ptr(), output.as_ptr()), pointers);
        assert_eq!(pool.available(), 1);
        let mut ended = false;
        for _ in 0..100_000 {
            match peer.read(&mut [0; 1]) {
                Ok(IoProgress::Closed) | Err(_) => {
                    ended = true;
                    break;
                }
                Ok(IoProgress::Pending) => std::thread::yield_now(),
                other => panic!("refusal emitted bytes: {other:?}"),
            }
        }
        assert!(ended);
    }
}

struct MovingClock(AtomicU64);
impl Clock for MovingClock {
    fn sample(&self) -> Result<Time, Error> {
        Ok(Time {
            utc_ms: 1_800_000_000_000,
            monotonic: Tick(self.0.load(Ordering::Relaxed)),
        })
    }
}

struct StepClock(AtomicU64);
impl Clock for StepClock {
    fn sample(&self) -> Result<Time, Error> {
        Ok(Time {
            utc_ms: 1_800_000_000_000,
            monotonic: Tick(self.0.fetch_add(1, Ordering::Relaxed)),
        })
    }
}

#[test]
fn server_starttls_deadlines_clock_refusal_and_cancel_release_reservations() {
    let _serial = serial();
    let material = Material::new();
    let lease = material.published(&source(false));
    let pool = HandshakePool::new(1).unwrap();
    let id = TlsPolicies::listener(&lease, "smtp").unwrap();
    let mut scratch = [0; LINE_BYTES];
    let mut reader = LineReader::new(&mut scratch).unwrap();
    reader.feed(b"STARTTLS\r\n").unwrap();
    for (whole, handshake, expected) in [(100, 0, Error::Deadline), (100, 101, Error::Invalid)] {
        let (_peer, plain) = sockets();
        let refusal = ServerStartTls::new(
            held(&lease, id, &pool),
            plain,
            &reader,
            &[],
            material.source.clone(),
            cap(whole),
            cap(handshake),
        )
        .err()
        .unwrap();
        assert_eq!(refusal.error(), expected);
        drop(refusal.into_buffers());
        assert_eq!(pool.available(), 1);
    }
    for after_write in [false, true] {
        let (_peer, plain) = sockets();
        let clock = Arc::new(MovingClock(AtomicU64::new(0)));
        let mut owner = ServerStartTls::new(
            held(&lease, id, &pool),
            plain,
            &reader,
            &[],
            clock.clone(),
            cap(1000),
            cap(100),
        )
        .ok()
        .unwrap();
        if after_write {
            let ServerUpgradeProgress::Pending(next) = owner.advance().ok().unwrap() else {
                panic!("handoff skipped flush")
            };
            owner = next;
        }
        clock.0.store(100, Ordering::Relaxed);
        let refusal = owner.advance().err().unwrap();
        assert_eq!(refusal.error(), Error::Deadline);
        drop(refusal.into_buffers());
        assert_eq!(pool.available(), 1);
    }
    // A single socket operation can cross the fixed deadline.
    let (_peer, plain) = sockets();
    let owner = ServerStartTls::new(
        held(&lease, id, &pool),
        plain,
        &reader,
        &[],
        Arc::new(StepClock(AtomicU64::new(0))),
        cap(1000),
        cap(2),
    )
    .ok()
    .unwrap();
    let refusal = owner.advance().err().unwrap();
    assert_eq!(refusal.error(), Error::Deadline);
    drop(refusal.into_buffers());
    assert_eq!(pool.available(), 1);
    let (_peer, plain) = sockets();
    let owner = ServerStartTls::new(
        held(&lease, id, &pool),
        plain,
        &reader,
        &[],
        material.source.clone(),
        cap(1000),
        cap(100),
    )
    .ok()
    .unwrap();
    material.source.0.store(true, Ordering::Relaxed);
    let refusal = owner.advance().err().unwrap();
    assert_eq!(refusal.error(), Error::Busy);
    drop(refusal.into_buffers());
    assert_eq!(pool.available(), 1);
    material.source.0.store(false, Ordering::Relaxed);
    let (_peer, plain) = sockets();
    let owner = ServerStartTls::new(
        held(&lease, id, &pool),
        plain,
        &reader,
        &[],
        material.source.clone(),
        cap(1000),
        cap(100),
    )
    .ok()
    .unwrap();
    assert_eq!(pool.available(), 0);
    drop(owner.into_buffers());
    assert_eq!(pool.available(), 1);
}
