use super::*;
use crate::{
    admission::{timers::NetworkLimits, DiskLimits, ViewMode, WorkLimits},
    limits::Limits,
    ports::{
        Deadline, FlushProgress, Handshake, IoProgress, PeerVerification, TlsTransport, Transport,
    },
    smtp_network::{Network, Progress as NetworkProgress},
    smtp_session::{Pending, Settings},
    smtp_starttls::{Progress, Upgrade},
    smtp_wire::{ReplyReader, LINE_BYTES},
    transport::TcpTransport,
};
use std::net::{TcpListener, TcpStream};

type Wire = Box<[u8; TLS_WIRE_BYTES]>;

fn sockets() -> (TcpTransport, TcpTransport) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    (
        TcpTransport::from_stream(client).unwrap(),
        TcpTransport::from_stream(server).unwrap(),
    )
}
fn cap(ms: u64) -> Deadline {
    Deadline::after(Tick(0), ms).unwrap()
}
fn held(
    lease: &GenerationLease<TlsPolicies>,
    id: TlsPolicyId,
    pool: &HandshakePool,
) -> SessionReservation<Wire> {
    let (input, output) = buffers();
    TlsPolicies::reserve_session(lease.clone(), id, pool, input, output)
        .unwrap()
        .construct()
        .unwrap()
}
fn fixture(clock: &dyn Clock, run: impl FnOnce(Network<'_>)) {
    let config = resolved(&source(false));
    let plan = NetworkLimits::default()
        .plan(
            &DiskLimits::default()
                .plan(
                    &Limits::default().plan().unwrap(),
                    WorkLimits::default(),
                    ViewMode::OnlineBackground,
                )
                .unwrap(),
        )
        .unwrap();
    config
        .candidate()
        .with_graph(|records, _| {
            run(Network::new(
                records.routes(),
                Settings {
                    hostname: "localhost",
                    message_bytes: 32768,
                    trace_bytes: 1024,
                    recipients: 100,
                    starttls: true,
                },
                &plan,
                clock,
            )
            .unwrap());
        })
        .unwrap();
}
fn send(client: &mut impl Transport, bytes: &[u8]) {
    let mut sent = 0;
    for _ in 0..100_000 {
        if sent < bytes.len() {
            match client.write(&bytes[sent..]).unwrap() {
                IoProgress::Bytes(n) => sent += n,
                IoProgress::Pending => (),
                IoProgress::Closed => panic!("closed while sending"),
            }
        } else if client.flush().unwrap() == FlushProgress::Complete {
            return;
        }
        std::thread::yield_now();
    }
    panic!("send did not finish");
}
fn exchange(
    network: &mut Network<'_>,
    server: &mut impl Transport,
    client: &mut impl Transport,
    clock: &dyn Clock,
    command: &[u8],
) -> Vec<u8> {
    send(client, command);
    let mut reply = Vec::new();
    let mut scratch = [0; LINE_BYTES];
    let mut reader = ReplyReader::new(&mut scratch).unwrap();
    let mut bytes = [0; 2048];
    for _ in 0..100_000 {
        assert_ne!(
            network.advance(server, clock).unwrap(),
            NetworkProgress::Work
        );
        client.flush().unwrap();
        if !reader.complete() {
            match client.read(&mut bytes).unwrap() {
                IoProgress::Bytes(n) => {
                    let mut used = 0;
                    while used < n {
                        let progress = reader.feed(&bytes[used..n]).unwrap();
                        assert!(progress.consumed > 0);
                        used += progress.consumed;
                        if reader.complete() {
                            assert_eq!(used, n);
                        } else if progress.complete {
                            reader.advance().unwrap();
                        }
                    }
                    reply.extend_from_slice(&bytes[..n]);
                }
                IoProgress::Pending => (),
                IoProgress::Closed => panic!("closed before reply"),
            }
        }
        if reader.complete() && network.session().pending() == Pending::Input {
            return reply;
        }
        std::thread::yield_now();
    }
    panic!("exchange did not finish");
}
fn request_upgrade<'a>(
    mut network: Network<'a>,
    server: &mut TcpTransport,
    client: &mut TcpTransport,
    clock: &dyn Clock,
) -> Network<'a> {
    assert!(exchange(&mut network, server, client, clock, b"").starts_with(b"220 "));
    assert!(
        exchange(&mut network, server, client, clock, b"EHLO sender.test\r\n")
            .windows(8)
            .any(|w| w == b"STARTTLS")
    );
    send(client, b"sTaRtTlS\r\n");
    for _ in 0..100_000 {
        if network.advance(server, clock).unwrap() == NetworkProgress::Work {
            assert_eq!(
                network.session().pending(),
                Pending::StartTls {
                    command: b"sTaRtTlS\r\n"
                }
            );
            return network;
        }
        std::thread::yield_now();
    }
    panic!("no STARTTLS handoff");
}
fn ready_reply<'a>(mut upgrade: Upgrade<'a, Wire>, client: &mut TcpTransport) -> Upgrade<'a, Wire> {
    let mut scratch = [0; LINE_BYTES];
    let mut reader = ReplyReader::new(&mut scratch).unwrap();
    let mut bytes = [0; LINE_BYTES];
    for _ in 0..100_000 {
        upgrade = match upgrade.advance().unwrap() {
            Progress::Pending(next) => next,
            Progress::Established { .. } => panic!("established without client TLS"),
        };
        match client.read(&mut bytes).unwrap() {
            IoProgress::Bytes(n) => {
                assert_eq!(reader.feed(&bytes[..n]).unwrap().consumed, n);
                if reader.complete() {
                    assert_eq!(reader.line().unwrap().code, 220);
                    return upgrade;
                }
            }
            IoProgress::Pending => (),
            IoProgress::Closed => panic!("closed before 220"),
        }
        std::thread::yield_now();
    }
    panic!("no STARTTLS ready reply");
}

fn establish<'a>(
    network: Network<'a>,
    lease: &GenerationLease<TlsPolicies>,
    pool: &HandshakePool,
    clock: Arc<dyn Clock>,
) -> (Network<'a>, TlsConnection<Wire>, TlsConnection<Wire>) {
    let original = network.deadline();
    let (mut client, mut server) = sockets();
    let network = request_upgrade(network, &mut server, &mut client, clock.as_ref());
    let upgrade = Upgrade::new(
        network,
        server,
        held(lease, TlsPolicies::listener(lease, "smtp").unwrap(), pool),
        clock.clone(),
        cap(100),
    )
    .unwrap();
    let upgrade = ready_reply(upgrade, &mut client);
    assert_eq!(pool.available(), 1);
    let mut client = held(lease, TlsPolicies::relay(lease).unwrap(), pool)
        .handoff(
            client,
            &[],
            clock.clone(),
            Deadline::after(original.tick(), 60_000).unwrap(),
            cap(100),
        )
        .unwrap();
    let mut upgrade = Some(upgrade);
    let mut established = None;
    let mut verified = false;
    for _ in 0..100_000 {
        if !verified {
            if let Handshake::Complete(info) = client.handshake(cap(100)).unwrap() {
                assert_eq!(info.peer, PeerVerification::ServerName);
                verified = true;
            }
        }
        if let Some(owner) = upgrade.take() {
            match owner.advance().unwrap() {
                Progress::Pending(next) => upgrade = Some(next),
                Progress::Established { network, transport } => {
                    established = Some((network, transport))
                }
            }
        }
        if verified && established.is_some() {
            break;
        }
        std::thread::yield_now();
    }
    assert!(verified);
    let (network, server) = established.unwrap();
    (network, server, client)
}

#[test]
fn receiving_starttls_resets_only_after_real_handshake_and_runs_encrypted_smtp() {
    let _serial = serial();
    let material = Material::new();
    let lease = material.published(&source(false));
    let pool = HandshakePool::new(2).unwrap();
    fixture(material.source.as_ref(), |network| {
        let original = network.deadline();
        let (mut network, mut server, mut client) =
            establish(network, &lease, &pool, material.source.clone());
        assert_eq!(pool.available(), 2);
        assert_eq!(network.deadline(), original);
        assert_eq!(server.info().unwrap().peer, PeerVerification::None);
        // No second SMTP banner is pending after the handshake.
        for _ in 0..8 {
            assert_eq!(
                network.advance(&mut server, material.source.as_ref()),
                Ok(NetworkProgress::Pending)
            );
            assert_eq!(client.read(&mut [0; 128]), Ok(IoProgress::Pending));
        }
        assert!(exchange(
            &mut network,
            &mut server,
            &mut client,
            material.source.as_ref(),
            b"MAIL FROM:<>\r\n"
        )
        .starts_with(b"503 "));
        let reply = exchange(
            &mut network,
            &mut server,
            &mut client,
            material.source.as_ref(),
            b"EHLO fresh.test\r\n",
        );
        assert!(!reply.windows(8).any(|w| w == b"STARTTLS"));
        assert!(exchange(
            &mut network,
            &mut server,
            &mut client,
            material.source.as_ref(),
            b"STARTTLS\r\n"
        )
        .starts_with(b"502 "));
        assert_eq!(
            exchange(
                &mut network,
                &mut server,
                &mut client,
                material.source.as_ref(),
                b"NOOP\r\n"
            ),
            b"250 2.0.0 OK\r\n"
        );
        network.abort(&mut server);
    });
}

#[test]
fn receiving_starttls_failure_cannot_resume_plaintext() {
    let _serial = serial();
    let material = Material::new();
    let lease = material.published(&source(false));
    let pool = HandshakePool::new(1).unwrap();
    fixture(material.source.as_ref(), |network| {
        let (mut client, mut server) = sockets();
        let network = request_upgrade(network, &mut server, &mut client, material.source.as_ref());
        let upgrade = Upgrade::new(
            network,
            server,
            held(
                &lease,
                TlsPolicies::listener(&lease, "smtp").unwrap(),
                &pool,
            ),
            material.source.clone(),
            cap(100),
        )
        .unwrap();
        let upgrade = ready_reply(upgrade, &mut client);
        send(&mut client, b"NOOP\r\n");
        let mut upgrade = Some(upgrade);
        let mut refused = false;
        for _ in 0..100_000 {
            match upgrade.take().unwrap().advance() {
                Ok(Progress::Pending(next)) => upgrade = Some(next),
                Ok(Progress::Established { .. }) => panic!("plaintext authorized TLS"),
                Err(_) => {
                    refused = true;
                    break;
                }
            }
            std::thread::yield_now();
        }
        assert!(refused);
        assert_eq!(pool.available(), 1);
        let mut bytes = [0; 128];
        for _ in 0..100_000 {
            match client.read(&mut bytes) {
                Ok(IoProgress::Closed) | Err(_) => return,
                Ok(IoProgress::Pending) => std::thread::yield_now(),
                other => panic!("plaintext resumed: {other:?}"),
            }
        }
        panic!("failed connection did not close");
    });
}

struct MovingClock(std::sync::atomic::AtomicU64);
impl Clock for MovingClock {
    fn sample(&self) -> Result<Time, Error> {
        Ok(Time {
            utc_ms: 1_800_000_000_000,
            monotonic: Tick(self.0.load(Ordering::Relaxed)),
        })
    }
}
fn ended(client: &mut TcpTransport) {
    let mut byte = [0];
    for _ in 0..100_000 {
        match client.read(&mut byte) {
            Ok(IoProgress::Closed) | Err(_) => return,
            Ok(IoProgress::Pending) => std::thread::yield_now(),
            other => panic!("refused upgrade sent plaintext: {other:?}"),
        }
    }
    panic!("refused connection stayed open");
}

#[test]
fn receiving_starttls_refuses_wrong_phase_and_enclosing_deadline_before_220() {
    let _serial = serial();
    let material = Material::new();
    let lease = material.published(&source(false));
    let pool = HandshakePool::new(1).unwrap();
    for bad_phase in [true, false] {
        fixture(material.source.as_ref(), |network| {
            let (mut client, mut server) = sockets();
            let network = if bad_phase {
                network
            } else {
                request_upgrade(network, &mut server, &mut client, material.source.as_ref())
            };
            let result = Upgrade::new(
                network,
                server,
                held(
                    &lease,
                    TlsPolicies::listener(&lease, "smtp").unwrap(),
                    &pool,
                ),
                material.source.clone(),
                cap(3_600_001),
            );
            assert_eq!(
                result.err(),
                Some(if bad_phase {
                    Error::Conflict
                } else {
                    Error::Invalid
                })
            );
            assert_eq!(pool.available(), 1);
            ended(&mut client);
        });
    }
}

#[test]
fn receiving_starttls_deadline_regression_and_cancellation_release_the_connection() {
    let _serial = serial();
    let material = Material::new();
    let lease = material.published(&source(false));
    let pool = HandshakePool::new(1).unwrap();
    for mode in 0..4 {
        let clock = Arc::new(MovingClock(std::sync::atomic::AtomicU64::new(1)));
        fixture(clock.as_ref(), |network| {
            let (mut client, mut server) = sockets();
            let network = request_upgrade(network, &mut server, &mut client, clock.as_ref());
            let upgrade = Upgrade::new(
                network,
                server,
                held(
                    &lease,
                    TlsPolicies::listener(&lease, "smtp").unwrap(),
                    &pool,
                ),
                clock.clone(),
                cap(100),
            )
            .unwrap();
            if mode == 3 {
                drop(upgrade);
                assert_eq!(pool.available(), 1);
                ended(&mut client);
                return;
            }
            let mut upgrade = ready_reply(upgrade, &mut client);
            for _ in 0..2 {
                upgrade = match upgrade.advance().unwrap() {
                    Progress::Pending(next) => next,
                    Progress::Established { .. } => panic!("established without client TLS"),
                };
            }
            assert_eq!(pool.available(), 0);
            if mode == 2 {
                drop(upgrade);
            } else {
                clock
                    .0
                    .store(if mode == 0 { 100 } else { 0 }, Ordering::Relaxed);
                assert_eq!(
                    upgrade.advance().err(),
                    Some(if mode == 0 {
                        Error::Deadline
                    } else {
                        Error::Invalid
                    })
                );
            }
            assert_eq!(pool.available(), 1);
            ended(&mut client);
        });
    }
}

fn to_work(
    network: &mut Network<'_>,
    server: &mut impl Transport,
    client: &mut impl Transport,
    clock: &dyn Clock,
    command: &[u8],
) {
    send(client, command);
    for _ in 0..100_000 {
        if network.advance(server, clock).unwrap() == NetworkProgress::Work {
            return;
        }
        std::thread::yield_now();
    }
    panic!("no storage handoff");
}
fn closing_reply(
    network: &mut Network<'_>,
    server: &mut impl Transport,
    client: &mut impl Transport,
    clock: &dyn Clock,
    expected: &[u8],
) {
    let mut reply = Vec::new();
    let mut bytes = [0; 256];
    for _ in 0..100_000 {
        network.advance(server, clock).unwrap();
        client.flush().unwrap();
        match client.read(&mut bytes).unwrap() {
            IoProgress::Bytes(n) => reply.extend_from_slice(&bytes[..n]),
            IoProgress::Pending => (),
            IoProgress::Closed => panic!("closed before final reply: {reply:?}"),
        }
        if reply == expected {
            return;
        }
        assert!(expected.starts_with(&reply));
        std::thread::yield_now();
    }
    panic!("missing closing reply: {reply:?}");
}

#[test]
fn established_tls_flushes_known_result_and_expiry_notice_within_fixed_finishing_cap() {
    let _serial = serial();
    let material = Material::new();
    let lease = material.published(&source(false));
    let pool = HandshakePool::new(2).unwrap();
    for commit in [false, true] {
        let clock = Arc::new(MovingClock(std::sync::atomic::AtomicU64::new(1)));
        fixture(clock.as_ref(), |network| {
            let original = network.deadline();
            let (mut network, mut server, mut client) =
                establish(network, &lease, &pool, clock.clone());
            if commit {
                for command in [
                    b"EHLO fresh.test\r\n".as_slice(),
                    b"MAIL FROM:<>\r\n",
                    b"RCPT TO:<main@example.test>\r\n",
                ] {
                    assert!(exchange(
                        &mut network,
                        &mut server,
                        &mut client,
                        clock.as_ref(),
                        command
                    )
                    .starts_with(b"250"));
                }
                to_work(
                    &mut network,
                    &mut server,
                    &mut client,
                    clock.as_ref(),
                    b"DATA\r\n",
                );
                network.data_ready(Ok(()), clock.as_ref()).unwrap();
                assert!(
                    exchange(&mut network, &mut server, &mut client, clock.as_ref(), b"")
                        .starts_with(b"354 ")
                );
                to_work(
                    &mut network,
                    &mut server,
                    &mut client,
                    clock.as_ref(),
                    b".\r\n",
                );
                assert_eq!(network.session().pending(), Pending::Commit);
            }
            clock.0.store(original.tick().0 + 1000, Ordering::Relaxed);
            if commit {
                // Protocol result injection; native durability is tested separately.
                network
                    .committed(
                        Ok(crate::ports::Commit {
                            account: crate::ids::AccountId::from_bytes([0xaa; 16]),
                            epoch: crate::ids::StoreEpoch::from_bytes([2; 16]),
                            sequence: crate::format::Sequence::from_u64(1),
                        }),
                        clock.as_ref(),
                    )
                    .unwrap();
            }
            let expected = if commit {
                b"250 2.0.0 Message accepted\r\n421 4.3.2 Service unavailable\r\n".as_slice()
            } else {
                b"421 4.3.2 Service unavailable\r\n"
            };
            closing_reply(
                &mut network,
                &mut server,
                &mut client,
                clock.as_ref(),
                expected,
            );
            assert_eq!(network.deadline(), original);
            // Let the final flush/close advance without relying on a TCP ACK.
            for _ in 0..8 {
                network.advance(&mut server, clock.as_ref()).unwrap();
            }
            clock.0.store(original.tick().0 + 5000, Ordering::Relaxed);
            assert_eq!(
                network.advance(&mut server, clock.as_ref()),
                Ok(NetworkProgress::Closed)
            );
        });
    }
}
