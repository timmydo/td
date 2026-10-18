use super::{deadline, Fixture};
use std::{
    net::{TcpListener, UdpSocket},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use td_mta::{
    clock::RuntimeClock,
    ports::{Clock, Deadline, FlushProgress, Handshake, IoProgress, TlsTransport, Transport},
    tls_admission::HandshakePool,
    tls_io::TLS_WIRE_BYTES,
    tls_policy::TlsPolicies,
    transport::TcpTransport,
};
pub(super) struct Relay {
    done: Arc<AtomicBool>,
    pub accepted: Arc<AtomicUsize>,
    pub interrupted: Arc<AtomicBool>,
    pub cancel_waiting: Arc<AtomicBool>,
    pub cancel_release: Arc<AtomicBool>,
    pub cancel_closed: Arc<AtomicBool>,
    threads: Vec<thread::JoinHandle<()>>,
}
impl Relay {
    pub(super) fn new(fixture: &Fixture, socket: TcpListener, dns: UdpSocket) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let accepted = Arc::new(AtomicUsize::new(0));
        let interrupted = Arc::new(AtomicBool::new(false));
        let mut domain = td_mta::generations::GenerationSet::at_startup();
        let trust = td_crypto::TrustStore::from_pem(&fixture.ca).unwrap();
        let epoch = td_mta::postgres_schema::inspect(&fixture.request(), &trust, deadline())
            .unwrap()
            .store_epoch;
        let profile = td_mta::postgres_runtime_profile::load(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            "cloud",
            domain.reserve().unwrap(),
        )
        .unwrap();
        let (_profile, retired) = profile.publish(&mut domain).ok().unwrap();
        drop(retired);
        let policies = domain.current().unwrap();
        socket.set_nonblocking(true).unwrap();
        dns.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let dns_done = done.clone();
        let dns_thread = thread::spawn(move || {
            while !dns_done.load(Ordering::Acquire) {
                let mut packet = [0; 512];
                let (n, peer) = match dns.recv_from(&mut packet) {
                    Ok(v) => v,
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        continue
                    }
                    Err(e) => panic!("{e}"),
                };
                let mut answer = packet[..n].to_vec();
                answer[2] |= 0x80;
                if packet[n - 3] == 1 {
                    answer[7] = 1;
                    answer.extend_from_slice(&[
                        0xc0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 127, 0, 0, 1,
                    ]);
                }
                dns.send_to(&answer, peer).unwrap();
            }
        });
        let server_done = done.clone();
        let delivered = accepted.clone();
        let interrupt = interrupted.clone();
        let cancel_waiting = Arc::new(AtomicBool::new(false));
        let cancel_release = Arc::new(AtomicBool::new(false));
        let cancel_closed = Arc::new(AtomicBool::new(false));
        let waiting = cancel_waiting.clone();
        let release = cancel_release.clone();
        let closed = cancel_closed.clone();
        let server = thread::spawn(move || {
            let mut connections = 0;
            while !server_done.load(Ordering::Acquire) {
                let socket = match socket.accept() {
                    Ok((s, _)) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                let clock = Arc::new(RuntimeClock::new());
                let cap = Deadline::after(clock.sample().unwrap().monotonic, 15000).unwrap();
                let pool = HandshakePool::new(1).unwrap();
                let mut peer = TlsPolicies::reserve_session(
                    policies.clone(),
                    TlsPolicies::listener(&policies, "smtp").unwrap(),
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
                .unwrap();
                while matches!(peer.handshake(cap).unwrap(), Handshake::Pending) {
                    thread::sleep(Duration::from_millis(1));
                }
                write(&mut peer, b"220 ready\r\n");
                assert_eq!(line(&mut peer), b"EHLO localhost\r\n");
                write(
                    &mut peer,
                    b"250-localhost\r\n250-8BITMIME\r\n250 AUTH PLAIN\r\n",
                );
                assert_eq!(line(&mut peer), b"AUTH PLAIN\r\n");
                write(&mut peer, b"334 \r\n");
                assert_eq!(
                    line(&mut peer),
                    b"AHJlbGF5LXVzZXIAZGF0YWJhc2UtcGFzc3dvcmQ=\r\n"
                );
                write(&mut peer, b"235 authenticated\r\n");
                let mail = line(&mut peer);
                assert!(mail.starts_with(b"MAIL FROM:<main@example.test>"));
                connections += 1;
                if connections == 2 {
                    assert!(mail
                        .windows(b"BODY=8BITMIME".len())
                        .any(|w| w == b"BODY=8BITMIME"));
                }
                if connections == 4 {
                    interrupt.store(true, Ordering::Release);
                    let end = Instant::now() + Duration::from_secs(15);
                    loop {
                        assert!(Instant::now() < end);
                        match peer.read(&mut [0; 512]) {
                            Ok(IoProgress::Closed) | Err(_) => break,
                            Ok(IoProgress::Pending) => thread::sleep(Duration::from_millis(1)),
                            other => panic!("unexpected after blocked MAIL: {other:?}"),
                        }
                    }
                    continue;
                }
                if connections == 7 {
                    waiting.store(true, Ordering::Release);
                    let end = Instant::now() + Duration::from_secs(15);
                    while !release.load(Ordering::Acquire) && !server_done.load(Ordering::Acquire) {
                        assert!(Instant::now() < end);
                        thread::sleep(Duration::from_millis(1));
                    }
                    write(&mut peer, b"250 sender\r\n");
                    loop {
                        assert!(Instant::now() < end);
                        match peer.read(&mut [0; 512]) {
                            Ok(IoProgress::Closed) | Err(_) => break,
                            Ok(IoProgress::Pending) => thread::sleep(Duration::from_millis(1)),
                            other => panic!("canceled worker sent SMTP bytes: {other:?}"),
                        }
                    }
                    closed.store(true, Ordering::Release);
                    continue;
                }
                if connections == 6 {
                    write(&mut peer, b"550 permanent fixture refusal\r\n");
                    continue;
                }
                if connections == 1 {
                    write(&mut peer, b"451 temporary fixture failure\r\n");
                    continue;
                }
                write(&mut peer, b"250 sender\r\n");
                assert_eq!(line(&mut peer), b"RCPT TO:<recipient@example.test>\r\n");
                write(&mut peer, b"250 recipient\r\n");
                assert_eq!(line(&mut peer), b"DATA\r\n");
                write(&mut peer, b"354 send\r\n");
                let mut body = Vec::new();
                loop {
                    let next = line(&mut peer);
                    if next == b".\r\n" {
                        break;
                    }
                    body.extend(next);
                    assert!(body.len() < 65536);
                }
                assert!(body.windows(8).any(|w| w == b"Subject:"));
                if connections == 2 {
                    assert!(body.windows(5).any(|w| w == "café".as_bytes()));
                }
                assert!(!body.windows(4).any(|w| w == b"Bcc:"));
                write(&mut peer, b"250 accepted\r\n");
                delivered.fetch_add(1, Ordering::Release);
            }
        });
        Self {
            done,
            accepted,
            interrupted,
            cancel_waiting,
            cancel_release,
            cancel_closed,
            threads: vec![dns_thread, server],
        }
    }
}
impl Drop for Relay {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
        for thread in self.threads.drain(..) {
            let joined = thread.join();
            if !std::thread::panicking() {
                assert!(joined.is_ok());
            }
        }
    }
}
#[track_caller]
fn line(peer: &mut super::relay_wire::Peer) -> Vec<u8> {
    let end = Instant::now() + Duration::from_secs(15);
    let mut result = Vec::new();
    loop {
        assert!(Instant::now() < end);
        let mut byte = [0];
        match peer.read(&mut byte).unwrap() {
            IoProgress::Bytes(1) => {
                result.push(byte[0]);
                if byte[0] == b'\n' {
                    return result;
                }
                assert!(result.len() < 2048);
            }
            IoProgress::Pending => thread::sleep(Duration::from_millis(1)),
            other => panic!("read: {other:?}"),
        }
    }
}
fn write(peer: &mut super::relay_wire::Peer, mut bytes: &[u8]) {
    let end = Instant::now() + Duration::from_secs(15);
    while !bytes.is_empty() {
        assert!(Instant::now() < end);
        if let IoProgress::Bytes(n) = peer.write(bytes).unwrap() {
            bytes = &bytes[n..];
        } else {
            thread::sleep(Duration::from_millis(1));
        }
    }
    while peer.flush().unwrap() != FlushProgress::Complete {
        assert!(Instant::now() < end);
        thread::sleep(Duration::from_millis(1));
    }
}
