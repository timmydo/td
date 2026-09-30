//! Private process peers for mail TLS fixtures, never a public crypto API.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use crate::{ClockHandle, PemCertificates, TrustStore, UtcClock, CERTIFICATE_DER_CAPACITY};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

struct FixtureClock;
impl UtcClock for FixtureClock {
    fn now(&self) -> Option<u64> {
        Some(1_800_000_000)
    }
}

#[test]
#[ignore = "invoked only by the mail process fixture with synthetic material"]
fn gateway_client_process() {
    let address: SocketAddr = std::env::var("TD_MTA_TEST_PEER_ADDRESS")
        .unwrap()
        .parse()
        .unwrap();
    assert!(address.ip().is_loopback());
    let directory = PathBuf::from(std::env::var_os("TD_MTA_TEST_PEER_MATERIAL").unwrap());
    let read = |name: &str| {
        let mut bytes = Vec::new();
        std::fs::File::open(directory.join(name))
            .unwrap()
            .take(131_073)
            .read_to_end(&mut bytes)
            .unwrap();
        assert!(bytes.len() <= 131_072);
        bytes
    };
    let trust = TrustStore::from_pem(&read("root.pem")).unwrap();
    let raw = read("client.pem");
    let mut certificates = PemCertificates::chain(&raw).unwrap();
    let mut chain = Vec::new();
    let mut scratch = [0; CERTIFICATE_DER_CAPACITY];
    while let Some(count) = certificates.decode_next(&mut scratch).unwrap() {
        chain.push(rustls::pki_types::CertificateDer::from(
            scratch[..count].to_vec(),
        ));
    }
    let key = crate::pem::private_key(&read("client-key.pem"), |der| {
        Ok(rustls::pki_types::PrivatePkcs8KeyDer::from(der.to_vec()))
    })
    .unwrap();
    let version = match std::env::var("TD_MTA_TEST_PEER_VERSION").unwrap().as_str() {
        "1.2" => &rustls::version::TLS12,
        "1.3" => &rustls::version::TLS13,
        _ => panic!("invalid fixture TLS version"),
    };
    let config = rustls::ClientConfig::builder_with_details(
        Arc::new(crate::tls_policy::provider().unwrap()),
        Arc::new(crate::tls_clock::BackendClock(Arc::new(ClockHandle::new(
            FixtureClock,
        )))),
    )
    .with_protocol_versions(&[version])
    .unwrap()
    .with_root_certificates(trust.roots.clone())
    .with_client_auth_cert(chain, key.into())
    .unwrap();
    let connection =
        rustls::ClientConnection::new(Arc::new(config), "localhost".try_into().unwrap()).unwrap();
    let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(3)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    match std::env::var("TD_MTA_TEST_PEER_TRANSPORT")
        .unwrap()
        .as_str()
    {
        "implicit" => {}
        "starttls" => {
            socket.write_all(b"STARTTLS\r\n").unwrap();
            socket.flush().unwrap();
            let mut reply = [0; b"220 2.0.0 Ready to start TLS\r\n".len()];
            socket.read_exact(&mut reply).unwrap();
            assert_eq!(&reply, b"220 2.0.0 Ready to start TLS\r\n");
        }
        _ => panic!("invalid fixture transport"),
    }
    let mut stream = rustls::StreamOwned::new(connection, socket);
    stream.write_all(b"EHLO gateway\r\n").unwrap();
    stream.flush().unwrap();
    let mut reply = [0; 8];
    stream.read_exact(&mut reply).unwrap();
    assert_eq!(&reply, b"250 OK\r\n");
    stream.conn.send_close_notify();
    stream.flush().unwrap();
}

#[test]
#[ignore = "invoked only by the memory controller with synthetic material"]
fn large_chain_server_process() {
    use std::net::TcpListener;
    let directory = PathBuf::from(std::env::var_os("TD_MTA_TEST_PEER_MATERIAL").unwrap());
    let read = |name: &str, limit: u64| {
        let mut bytes = Vec::new();
        std::fs::File::open(directory.join(name))
            .unwrap()
            .take(limit + 1)
            .read_to_end(&mut bytes)
            .unwrap();
        assert!(bytes.len() as u64 <= limit);
        bytes
    };
    // A remote fixture can exceed our local PEM input ceiling without changing it.
    let mut chain = Vec::new();
    let mut total = 0;
    for i in 0..4 {
        let bytes = read(&format!("cert{i}.der"), 16 * 1024);
        total += bytes.len();
        chain.push(rustls::pki_types::CertificateDer::from(bytes));
    }
    assert!(total > 63 * 1024 && total <= 65_000);
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(read("server-key.der", 4096));
    let version = match std::env::var("TD_MTA_TEST_PEER_VERSION").unwrap().as_str() {
        "1.2" => &rustls::version::TLS12,
        "1.3" => &rustls::version::TLS13,
        _ => panic!("invalid fixture TLS version"),
    };
    let mut config = rustls::ServerConfig::builder_with_details(
        Arc::new(crate::tls_policy::provider().unwrap()),
        Arc::new(crate::tls_clock::BackendClock(Arc::new(ClockHandle::new(
            FixtureClock,
        )))),
    )
    .with_protocol_versions(&[version])
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(chain, key.into())
    .unwrap();
    config.send_tls13_tickets = 2;
    let tickets = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let tls13 = version.version == rustls::ProtocolVersion::TLSv1_3;
    if tls13 {
        config.ticketer = Arc::new(LargeOpaqueTickets(tickets.clone()));
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    std::fs::write(
        directory.join("address.tmp"),
        listener.local_addr().unwrap().to_string(),
    )
    .unwrap();
    std::fs::rename(directory.join("address.tmp"), directory.join("address")).unwrap();
    let start = std::time::Instant::now();
    let socket = loop {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "client never connected"
        );
        match listener.accept() {
            Ok((socket, peer)) => {
                assert!(peer.ip().is_loopback());
                break socket;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(e) => panic!("fixture accept: {e}"),
        }
    };
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let connection = rustls::ServerConnection::new(Arc::new(config)).unwrap();
    let mut stream = rustls::StreamOwned::new(connection, socket);
    let mut bytes = [0; 16_384];
    for value in 0..=32u8 {
        stream.read_exact(&mut bytes).unwrap();
        assert_eq!(
            tickets.load(std::sync::atomic::Ordering::Relaxed),
            if tls13 { 2 } else { 0 }
        );
        assert!(bytes.iter().all(|&byte| byte == value));
        stream.write_all(&bytes).unwrap();
        stream.flush().unwrap();
    }
    stream.conn.send_close_notify();
    stream.flush().unwrap();
}

#[derive(Debug)]
struct LargeOpaqueTickets(Arc<std::sync::atomic::AtomicUsize>);
impl rustls::server::ProducesTickets for LargeOpaqueTickets {
    fn enabled(&self) -> bool {
        true
    }
    fn lifetime(&self) -> u32 {
        3600
    }
    fn encrypt(&self, _: &[u8]) -> Option<Vec<u8>> {
        // Opaque test payload, not encryption and never accepted for resumption.
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(vec![0x5a; 16_000])
    }
    fn decrypt(&self, _: &[u8]) -> Option<Vec<u8>> {
        None
    }
}
