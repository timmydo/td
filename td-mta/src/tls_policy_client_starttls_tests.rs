use super::*;
use crate::{
    ports::{Deadline, Handshake, IoProgress, PeerVerification, TlsTransport, Transport},
    smtp_wire::{EhloReader, LineReader, StartTlsOffer, LINE_BYTES},
    transport::TcpTransport,
};
use std::net::{TcpListener, TcpStream};

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
fn configuration() -> String {
    source(false).replace(
        "port = 465\n",
        "port = 587\ntransport = \"required_starttls\"\n",
    )
}
fn offer() -> StartTlsOffer {
    let mut scratch = [0; LINE_BYTES];
    let mut reader = EhloReader::new(&mut scratch).unwrap();
    reader.feed(b"250-localhost\r\n").unwrap();
    reader.advance().unwrap();
    reader.feed(b"250 STARTTLS\r\n").unwrap();
    reader.into_starttls_offer(&[]).unwrap()
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
fn command(peer: &mut TcpTransport) {
    let mut bytes = [0; 32];
    let mut used = 0;
    for _ in 0..100_000 {
        match peer.read(&mut bytes[used..]).unwrap() {
            IoProgress::Bytes(n) => {
                used += n;
                if used >= 10 {
                    break;
                }
            }
            IoProgress::Pending => std::thread::yield_now(),
            IoProgress::Closed => panic!("closed before command"),
        }
    }
    assert_eq!(&bytes[..used], b"STARTTLS\r\n");
}
fn waiting<'a>(
    owner: ClientStartTls<'a, Box<[u8; TLS_WIRE_BYTES]>>,
) -> ClientStartTls<'a, Box<[u8; TLS_WIRE_BYTES]>> {
    match owner.advance().ok().unwrap() {
        ClientUpgradeProgress::Pending(next) => next,
        ClientUpgradeProgress::Tls(_) => panic!("TLS before complete 220"),
    }
}

#[test]
pub(super) fn client_starttls_and_server_upgrade_verify_real_tls() {
    let _serial = serial();
    let material = Material::new();
    let lease = material.published(&configuration());
    let pool = HandshakePool::new(2).unwrap();
    let (plain, mut peer) = sockets();
    let mut input = [0; LINE_BYTES];
    let mut reply = [0; LINE_BYTES];
    let owner = ClientStartTls::new(
        offer(),
        held(&lease, TlsPolicies::relay(&lease).unwrap(), &pool),
        plain,
        UpgradeScratch {
            input: &mut input,
            reply: &mut reply,
        },
        material.source.clone(),
        cap(1000),
        cap(100),
    )
    .ok()
    .unwrap();
    let mut owner = Some(waiting(waiting(owner)));
    command(&mut peer);
    let mut scratch = [0; LINE_BYTES];
    let mut reader = LineReader::new(&mut scratch).unwrap();
    reader.feed(b"STARTTLS\r\n").unwrap();
    let mut server_owner = Some(
        ServerStartTls::new(
            held(
                &lease,
                TlsPolicies::listener(&lease, "smtp").unwrap(),
                &pool,
            ),
            peer,
            &reader,
            &[],
            material.source.clone(),
            cap(1000),
            cap(100),
        )
        .ok()
        .unwrap(),
    );
    let (mut client, mut server) = (None, None);
    for _ in 0..100_000 {
        if let Some(current) = server_owner.take() {
            match current.advance().ok().unwrap() {
                ServerUpgradeProgress::Pending(next) => server_owner = Some(next),
                ServerUpgradeProgress::Tls(tls) => server = Some(tls),
            }
        }
        if let Some(current) = owner.take() {
            match current.advance().ok().unwrap() {
                ClientUpgradeProgress::Pending(next) => owner = Some(next),
                ClientUpgradeProgress::Tls(tls) => client = Some(tls),
            }
        }
        if client.is_some() && server.is_some() {
            break;
        }
        std::thread::yield_now();
    }
    let (mut client, mut server) = (client.unwrap(), server.unwrap());
    assert_eq!(pool.available(), 0);
    assert_eq!(client.info(), None);
    let mut complete = false;
    for _ in 0..100_000 {
        if let (Handshake::Complete(c), Handshake::Complete(s)) = (
            client.handshake(cap(100)).unwrap(),
            server.handshake(cap(100)).unwrap(),
        ) {
            assert_eq!(c.peer, PeerVerification::ServerName);
            assert_eq!(s.peer, PeerVerification::None);
            complete = true;
            break;
        }
        std::thread::yield_now();
    }
    assert!(complete);
    assert_eq!(pool.available(), 2);
    drop(client.into_buffers().unwrap());
    drop(server.into_buffers().unwrap());
}

#[test]
pub(super) fn client_starttls_refuses_bad_replies_and_accepts_fragmented_220() {
    let _serial = serial();
    let material = Material::new();
    let lease = material.published(&configuration());
    let pool = HandshakePool::new(1).unwrap();
    let oversized = [b"220 ".as_slice(), &[b'x'; LINE_BYTES], b"\r\n"].concat();
    for (response, expected) in [
        (b"454 unavailable\r\n".as_slice(), Error::Tls),
        (b"250 OK\r\n", Error::Tls),
        (b"354 continue\r\n", Error::Tls),
        (b"220-first\r\n250 last\r\n", Error::Invalid),
        (b"220 bad\n", Error::Invalid),
        (b"220 caf\xc3\xa9\r\n", Error::Invalid),
        (b"220 ready\r\nNOOP\r\n", Error::Invalid),
        (b"220 ready\r\n\x16\x03\x03", Error::Invalid),
        (oversized.as_slice(), Error::Capacity),
    ] {
        let (plain, mut peer) = sockets();
        let mut input = [0; LINE_BYTES];
        let mut reply = [0; LINE_BYTES];
        let prepared = held(&lease, TlsPolicies::relay(&lease).unwrap(), &pool);
        let owner = ClientStartTls::new(
            offer(),
            prepared,
            plain,
            UpgradeScratch {
                input: &mut input,
                reply: &mut reply,
            },
            material.source.clone(),
            cap(1000),
            cap(100),
        )
        .ok()
        .unwrap();
        let mut owner = Some(waiting(waiting(owner)));
        command(&mut peer);
        assert_eq!(peer.write(response), Ok(IoProgress::Bytes(response.len())));
        let mut refusal = None;
        for _ in 0..100_000 {
            match owner.take().unwrap().advance() {
                Ok(ClientUpgradeProgress::Pending(next)) => owner = Some(next),
                Ok(ClientUpgradeProgress::Tls(_)) => panic!("accepted bad reply"),
                Err(error) => {
                    refusal = Some(error);
                    break;
                }
            }
            std::thread::yield_now();
        }
        let refusal = refusal.unwrap();
        assert_eq!(refusal.error(), expected);
        drop(refusal.into_buffers());
        assert_eq!(pool.available(), 1);
    }
    // EOF cannot turn a partial positive reply into permission for TLS.
    let (plain, mut peer) = sockets();
    let mut input = [0; LINE_BYTES];
    let mut reply = [0; LINE_BYTES];
    let owner = ClientStartTls::new(
        offer(),
        held(&lease, TlsPolicies::relay(&lease).unwrap(), &pool),
        plain,
        UpgradeScratch {
            input: &mut input,
            reply: &mut reply,
        },
        material.source.clone(),
        cap(1000),
        cap(100),
    )
    .ok()
    .unwrap();
    let mut owner = Some(waiting(waiting(owner)));
    command(&mut peer);
    assert_eq!(peer.write(b"220 ready"), Ok(IoProgress::Bytes(9)));
    drop(peer);
    let mut ended = false;
    for _ in 0..100_000 {
        match owner.take().unwrap().advance() {
            Ok(ClientUpgradeProgress::Pending(next)) => owner = Some(next),
            Ok(ClientUpgradeProgress::Tls(_)) => panic!("TLS after truncated 220"),
            Err(refusal) => {
                assert_eq!(refusal.error(), Error::Tls);
                drop(refusal.into_buffers());
                ended = true;
                break;
            }
        }
        std::thread::yield_now();
    }
    assert!(ended);
    assert_eq!(pool.available(), 1);
    for response in [b"220\r\n".as_slice(), b"220-first\r\n220 ready\r\n"] {
        let (plain, mut peer) = sockets();
        let mut input = [0; LINE_BYTES];
        let mut reply = [0; LINE_BYTES];
        let owner = ClientStartTls::new(
            offer(),
            held(&lease, TlsPolicies::relay(&lease).unwrap(), &pool),
            plain,
            UpgradeScratch {
                input: &mut input,
                reply: &mut reply,
            },
            material.source.clone(),
            cap(1000),
            cap(100),
        )
        .ok()
        .unwrap();
        let mut owner = Some(waiting(waiting(owner)));
        command(&mut peer);
        let mut tls = None;
        for (index, byte) in response.iter().enumerate() {
            assert_eq!(peer.write(&[*byte]), Ok(IoProgress::Bytes(1)));
            // One read may be Pending; repeat only after all bytes are queued.
            match owner.take().unwrap().advance().ok().unwrap() {
                ClientUpgradeProgress::Pending(next) => owner = Some(next),
                ClientUpgradeProgress::Tls(connection) => {
                    assert_eq!(index + 1, response.len());
                    tls = Some(connection);
                }
            }
        }
        for _ in 0..100_000 {
            if tls.is_some() {
                break;
            }
            match owner.take().unwrap().advance().ok().unwrap() {
                ClientUpgradeProgress::Pending(next) => owner = Some(next),
                ClientUpgradeProgress::Tls(connection) => tls = Some(connection),
            }
            std::thread::yield_now();
        }
        let tls = tls.unwrap();
        assert_eq!(tls.info(), None);
        assert_eq!(peer.read(&mut [0; 1]), Ok(IoProgress::Pending));
        drop(tls.into_buffers().unwrap());
        assert_eq!(pool.available(), 1);
    }
}

#[test]
pub(super) fn client_starttls_roles_scratch_deadlines_and_cancel_return_buffers() {
    let _serial = serial();
    let material = Material::new();
    let pool = HandshakePool::new(1).unwrap();
    for (config, role, scratch_size, deadline, handshake, expected) in [
        (
            source(false),
            "relay",
            LINE_BYTES,
            1000,
            100,
            Error::Invalid,
        ),
        (
            configuration(),
            "smtp",
            LINE_BYTES,
            1000,
            100,
            Error::Invalid,
        ),
        (
            configuration(),
            "relay",
            LINE_BYTES - 1,
            1000,
            100,
            Error::Capacity,
        ),
        (
            configuration(),
            "relay",
            LINE_BYTES,
            1000,
            0,
            Error::Deadline,
        ),
        (
            configuration(),
            "relay",
            LINE_BYTES,
            10,
            100,
            Error::Invalid,
        ),
    ] {
        let lease = material.published(&config);
        let id = if role == "relay" {
            TlsPolicies::relay(&lease).unwrap()
        } else {
            TlsPolicies::listener(&lease, role).unwrap()
        };
        let (plain, _peer) = sockets();
        let (input, output) = buffers();
        let pointers = (input.as_ptr(), output.as_ptr());
        let prepared = TlsPolicies::reserve_session(lease, id, &pool, input, output)
            .unwrap()
            .construct()
            .unwrap();
        let mut input = [0; LINE_BYTES];
        let mut reply = [0; LINE_BYTES];
        let refusal = ClientStartTls::new(
            offer(),
            prepared,
            plain,
            UpgradeScratch {
                input: &mut input[..scratch_size],
                reply: &mut reply,
            },
            material.source.clone(),
            cap(deadline),
            cap(handshake),
        )
        .err()
        .unwrap();
        assert_eq!(refusal.error(), expected);
        let (input, output) = refusal.into_buffers();
        assert_eq!((input.as_ptr(), output.as_ptr()), pointers);
        assert_eq!(pool.available(), 1);
    }
    let lease = material.published(&configuration());
    // Expiry after the write still refuses the owner and releases capacity.
    let (plain, _peer) = sockets();
    let mut input = [0; LINE_BYTES];
    let mut reply = [0; LINE_BYTES];
    let owner = ClientStartTls::new(
        offer(),
        held(&lease, TlsPolicies::relay(&lease).unwrap(), &pool),
        plain,
        UpgradeScratch {
            input: &mut input,
            reply: &mut reply,
        },
        Arc::new(StepClock(std::sync::atomic::AtomicU64::new(0))),
        cap(1000),
        cap(2),
    )
    .ok()
    .unwrap();
    let refusal = owner.advance().err().unwrap();
    assert_eq!(refusal.error(), Error::Deadline);
    drop(refusal.into_buffers());
    assert_eq!(pool.available(), 1);
    // Reply assembly has its own reservation check.
    let (plain, _peer) = sockets();
    let refusal = ClientStartTls::new(
        offer(),
        held(&lease, TlsPolicies::relay(&lease).unwrap(), &pool),
        plain,
        UpgradeScratch {
            input: &mut input,
            reply: &mut reply[..LINE_BYTES - 1],
        },
        material.source.clone(),
        cap(1000),
        cap(100),
    )
    .err()
    .unwrap();
    assert_eq!(refusal.error(), Error::Capacity);
    drop(refusal.into_buffers());
    assert_eq!(pool.available(), 1);
    for fail_clock in [false, true] {
        let (plain, mut peer) = sockets();
        let mut input = [0; LINE_BYTES];
        let mut reply = [0; LINE_BYTES];
        let owner = ClientStartTls::new(
            offer(),
            held(&lease, TlsPolicies::relay(&lease).unwrap(), &pool),
            plain,
            UpgradeScratch {
                input: &mut input,
                reply: &mut reply,
            },
            material.source.clone(),
            cap(1000),
            cap(100),
        )
        .ok()
        .unwrap();
        let owner = waiting(waiting(owner));
        command(&mut peer);
        if fail_clock {
            material.source.0.store(true, Ordering::Relaxed);
            let refusal = owner.advance().err().unwrap();
            assert_eq!(refusal.error(), Error::Busy);
            drop(refusal.into_buffers());
            material.source.0.store(false, Ordering::Relaxed);
        } else {
            drop(owner.into_buffers());
        }
        assert_eq!(pool.available(), 1);
    }
}

struct StepClock(std::sync::atomic::AtomicU64);
impl Clock for StepClock {
    fn sample(&self) -> Result<Time, Error> {
        Ok(Time {
            utc_ms: 1_800_000_000_000,
            monotonic: Tick(self.0.fetch_add(1, Ordering::Relaxed)),
        })
    }
}
