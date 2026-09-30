//! Private process peer for mail mTLS fixtures, never a public crypto API.
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
    let socket = TcpStream::connect_timeout(&address, Duration::from_secs(3)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut stream = rustls::StreamOwned::new(connection, socket);
    stream.write_all(b"EHLO gateway\r\n").unwrap();
    stream.flush().unwrap();
    let mut reply = [0; 8];
    stream.read_exact(&mut reply).unwrap();
    assert_eq!(&reply, b"250 OK\r\n");
    stream.conn.send_close_notify();
    stream.flush().unwrap();
}
