use super::*;
use crate::{
    ports::{
        Deadline, FlushProgress, Handshake, IoProgress, PeerVerification, TlsTransport, Transport,
    },
    tls_policy::TlsConnection,
    transport::TcpTransport,
};
use std::{
    net::{TcpListener, TcpStream},
    sync::atomic::AtomicU64,
};

type Connection = TlsConnection<Box<[u8; TLS_WIRE_BYTES]>>;

fn sockets() -> (TcpTransport, TcpTransport) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let left = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (right, _) = listener.accept().unwrap();
    (
        TcpTransport::from_stream(left).unwrap(),
        TcpTransport::from_stream(right).unwrap(),
    )
}
fn cap(tick: u64) -> Deadline {
    Deadline::after(Tick(0), tick).unwrap()
}
fn reservation(
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
fn pair(
    material: &Material,
    lease: &GenerationLease<TlsPolicies>,
    pool: &HandshakePool,
    server_name: &str,
) -> (Connection, Connection) {
    let (left, right) = sockets();
    let make = |stream, id| {
        reservation(lease, id, pool)
            .handoff(stream, &[], material.source.clone(), cap(1000), cap(100))
            .unwrap()
    };
    (
        make(left, TlsPolicies::relay(lease).unwrap()),
        make(right, TlsPolicies::listener(lease, server_name).unwrap()),
    )
}
fn complete(client: &mut Connection, server: &mut Connection) -> Result<(), Error> {
    for _ in 0..100_000 {
        let c = client.handshake(cap(100))?;
        let s = server.handshake(cap(100))?;
        if matches!((c, s), (Handshake::Complete(_), Handshake::Complete(_))) {
            return Ok(());
        }
        std::thread::yield_now();
    }
    panic!("local retained handshake did not finish")
}

#[test]
fn handoff_refuses_plaintext_tails_and_deadlines_and_returns_original_buffers() {
    let _serial = serial();
    let material = Material::new();
    let lease = material.published(&source(false));
    let id = TlsPolicies::relay(&lease).unwrap();
    let pool = HandshakePool::new(1).unwrap();
    for mode in 0..4 {
        let (plain, mut peer) = sockets();
        let (input, output) = buffers();
        let pointers = (input.as_ptr(), output.as_ptr());
        let held = TlsPolicies::reserve_session(lease.clone(), id, &pool, input, output)
            .unwrap()
            .construct()
            .unwrap();
        assert_eq!(pool.available(), 0);
        if mode == 3 {
            material.source.0.store(true, Ordering::Relaxed);
        }
        let result = held.handoff(
            plain,
            if mode == 0 { b"MAIL FROM" } else { b"" },
            material.source.clone(),
            cap(100),
            cap(if mode == 1 {
                0
            } else if mode == 2 {
                101
            } else {
                10
            }),
        );
        let refusal = result.err().unwrap();
        assert_eq!(
            refusal.error(),
            match mode {
                1 => Error::Deadline,
                3 => Error::Busy,
                _ => Error::Invalid,
            }
        );
        let (input, output) = refusal.into_buffers();
        assert_eq!((input.as_ptr(), output.as_ptr()), pointers);
        assert_eq!(pool.available(), 1);
        let mut ended = false;
        for _ in 0..10_000 {
            match peer.read(&mut [0; 1]) {
                Ok(IoProgress::Closed) | Err(_) => {
                    ended = true;
                    break;
                }
                Ok(IoProgress::Pending) => std::thread::yield_now(),
                other => panic!("refused handoff wrote bytes: {other:?}"),
            }
        }
        assert!(ended);
        material.source.0.store(false, Ordering::Relaxed);
    }
}

#[test]
fn authorized_connections_release_handshake_capacity_and_retain_generation_until_teardown() {
    let _serial = serial();
    let material = Material::new();
    let mut set = GenerationSet::at_startup();
    drop(
        set.publish(material.prepare(&set, &source(false)).unwrap())
            .unwrap(),
    );
    let lease = set.current().unwrap();
    let pool = HandshakePool::new(2).unwrap();
    let (mut client, mut server) = pair(&material, &lease, &pool, "smtp");
    drop(lease);
    drop(
        set.publish(material.prepare(&set, &source(false)).unwrap())
            .unwrap(),
    );
    assert_eq!(set.available(), 0);
    assert_eq!(pool.available(), 0);
    assert_eq!(client.info(), None);
    assert_eq!(server.info(), None);
    assert_eq!(client.write(b"not yet"), Ok(IoProgress::Pending));
    let mut untouched = [0xa5; 16];
    assert_eq!(server.read(&mut untouched), Ok(IoProgress::Pending));
    assert_eq!(untouched, [0xa5; 16]);
    complete(&mut client, &mut server).unwrap();
    assert_eq!(pool.available(), 2);
    assert_eq!(client.info().unwrap().peer, PeerVerification::ServerName);
    assert_eq!(server.info().unwrap().peer, PeerVerification::None);
    let message = b"EHLO localhost\r\n";
    let mut sent = 0;
    let mut received = Vec::new();
    for _ in 0..100_000 {
        if sent < message.len() {
            if let IoProgress::Bytes(n) = client.write(&message[sent..]).unwrap() {
                sent += n;
            }
        }
        client.flush().unwrap();
        let mut chunk = [0; 4];
        if let IoProgress::Bytes(n) = server.read(&mut chunk).unwrap() {
            received.extend_from_slice(&chunk[..n]);
        }
        if received.len() == message.len() {
            break;
        }
        std::thread::yield_now();
    }
    assert_eq!(received, message);
    let mut closed = false;
    for _ in 0..100_000 {
        if client.close().unwrap() == FlushProgress::Complete {
            closed = true;
            break;
        }
        server.flush().unwrap();
    }
    assert!(closed);
    assert_eq!(client.close(), Ok(FlushProgress::Complete));
    let mut eof = false;
    for _ in 0..100_000 {
        if server.read(&mut [0; 1]).unwrap() == IoProgress::Closed {
            eof = true;
            break;
        }
        std::thread::yield_now();
    }
    assert!(eof);
    assert_eq!(set.available(), 0);
    drop(client.into_buffers().unwrap());
    assert_eq!(set.available(), 0);
    drop(server.into_buffers().unwrap());
    assert_eq!(set.available(), 1);
}

#[test]
fn gateway_without_client_certificate_never_exposes_mail_proof_and_recovers_permits() {
    let _serial = serial();
    let material = Material::new();
    let lease = material.published(&source(true));
    let pool = HandshakePool::new(2).unwrap();
    let (mut client, mut server) = pair(&material, &lease, &pool, "gateway");
    assert_eq!(complete(&mut client, &mut server), Err(Error::Tls));
    assert_eq!(server.info(), None);
    assert_eq!(server.write(b"250 accepted"), Err(Error::Tls));
    assert_eq!(server.read(&mut []), Ok(IoProgress::Pending));
    client.abort();
    assert_eq!(pool.available(), 2);
    drop(client.into_buffers().unwrap());
    drop(server.into_buffers().unwrap());
}

struct AdvancingClock(AtomicU64);
impl Clock for AdvancingClock {
    fn sample(&self) -> Result<Time, Error> {
        Ok(Time {
            utc_ms: 1_800_000_000_000,
            monotonic: Tick(self.0.load(Ordering::Relaxed)),
        })
    }
}

#[test]
fn handshake_deadline_can_only_tighten_and_failure_is_sticky() {
    let _serial = serial();
    let mut material = Material::new();
    let clock = Arc::new(AdvancingClock(AtomicU64::new(0)));
    material.clock = Arc::new(ClockHandle::new(TlsClockSource::new(clock.clone())));
    let lease = material.published(&source(false));
    let pool = HandshakePool::new(1).unwrap();
    let id = TlsPolicies::listener(&lease, "smtp").unwrap();
    let (plain, _peer) = sockets();
    let mut connection = reservation(&lease, id, &pool)
        .handoff(plain, &[], clock.clone(), cap(1000), cap(100))
        .unwrap();
    assert_eq!(connection.handshake(cap(10)), Ok(Handshake::Pending));
    assert_eq!(connection.handshake(cap(500)), Ok(Handshake::Pending));
    assert_eq!(pool.available(), 0);
    clock.0.store(10, Ordering::Relaxed);
    assert_eq!(connection.handshake(cap(500)), Err(Error::Deadline));
    assert_eq!(pool.available(), 1);
    clock.0.store(0, Ordering::Relaxed);
    assert_eq!(connection.handshake(cap(500)), Err(Error::Deadline));
    assert_eq!(connection.info(), None);
    assert_eq!(connection.flush(), Err(Error::Deadline));
    assert_eq!(connection.write(&[]), Ok(IoProgress::Pending));
    drop(connection.into_buffers().unwrap());
}

#[test]
fn changed_gateway_policy_aborts_pending_connection_and_releases_capacity() {
    let _serial = serial();
    let material = Material::new();
    let mut set = GenerationSet::at_startup();
    drop(
        set.publish(material.prepare(&set, &source(true)).unwrap())
            .unwrap(),
    );
    let old = set.current().unwrap();
    let pool = HandshakePool::new(1).unwrap();
    let id = TlsPolicies::listener(&old, "gateway").unwrap();
    let (plain, _peer) = sockets();
    let mut connection = reservation(&old, id, &pool)
        .handoff(plain, &[], material.source.clone(), cap(1000), cap(100))
        .unwrap();
    connection.check_gateway_policy(&old).unwrap(); // Comparison only: caller supplies freshness.
    let changed = source(true).replace("192.0.2.0/24", "192.0.0.0/16");
    drop(
        set.publish(material.prepare(&set, &changed).unwrap())
            .unwrap(),
    );
    assert_eq!(
        connection.check_gateway_policy(&set.current().unwrap()),
        Err(Error::Forbidden)
    );
    assert_eq!(pool.available(), 1);
    assert_eq!(connection.check_gateway_policy(&old), Err(Error::Forbidden));
    assert_eq!(connection.info(), None);
    assert_eq!(connection.handshake(cap(100)), Err(Error::Forbidden));
    drop(connection.into_buffers().unwrap());
}

#[test]
fn established_clock_failure_clears_cached_authorization_and_aborts_socket() {
    let _serial = serial();
    let material = Material::new();
    let lease = material.published(&source(false));
    let pool = HandshakePool::new(2).unwrap();
    let (mut client, mut server) = pair(&material, &lease, &pool, "smtp");
    complete(&mut client, &mut server).unwrap();
    material.source.0.store(true, Ordering::Relaxed);
    assert_eq!(client.flush(), Err(Error::Busy));
    assert_eq!(client.info(), None);
    material.source.0.store(false, Ordering::Relaxed);
    assert_eq!(client.handshake(cap(100)), Err(Error::Busy));
    assert_eq!(pool.available(), 2);
}
