//! One local TLS 1.3 pair; observations are not worst-case admission bounds.
#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
#[path = "tls_certificate_fixture.rs"]
mod certificate_fixture;
use certificate_fixture::{certificate_names, certificate_with, pem, Certificate};
use std::{
    hint::black_box,
    io::Cursor,
    net::{TcpListener, TcpStream},
    sync::Arc,
};
use td_crypto::{ClockHandle, Crypto, Provider, P256_PKCS8_CAPACITY};
use td_mta::{
    clock::TlsClockSource,
    config::{load, materialize, stanza, storage, stream},
    generations::GenerationSet,
    ports::{
        Clock, Deadline, Error, FlushProgress, Handshake, IoProgress, PeerVerification, Tick, Time,
        TlsTransport, TlsVersion, Transport,
    },
    tls_admission::HandshakePool,
    tls_io::TLS_WIRE_BYTES,
    tls_policy::{MaterialKind, TlsConnection, TlsPolicies},
    transport::TcpTransport,
};

pub const PHASES: [&str; 13] = [
    "baseline",
    "material",
    "config",
    "generation",
    "buffers",
    "constructed",
    "handshake",
    "record",
    "repeated",
    "client_released",
    "released",
    "buffers_released",
    "dropped",
];
const SOURCE: &str = r#"version = 1
[server]
hostname = "localhost"
jmap_origin = "https://localhost"
[account "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
username = "private-user"
[domain "example.test"]
[alias "main@example.test"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[identity "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
email = "main@example.test"
[resolver "primary"]
address = "127.0.0.1:53"
[relay]
host = "localhost"
port = 465
username = "private-relay"
password_file = "/password"
ca_file = "/root"
[certificate "public"]
mode = "files"
chain_file = "/chain"
key_file = "/key"
[listener "smtp"]
kind = "direct_smtp"
bind = "127.0.0.1:25"
server_name = "localhost"
certificate = "public"
session_limit = 1
per_peer_limit = 1
[listener "https"]
kind = "https"
bind = "127.0.0.1:443"
certificate = "public"
"#;
struct FixedClock;
impl Clock for FixedClock {
    fn sample(&self) -> Result<Time, Error> {
        Ok(Time {
            utc_ms: 1_800_000_000_000,
            monotonic: Tick(0),
        })
    }
}
type Buffer = Box<[u8; TLS_WIRE_BYTES]>;
fn buffer() -> Buffer {
    vec![0; TLS_WIRE_BYTES]
        .into_boxed_slice()
        .try_into()
        .unwrap()
}
fn material() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut raw = [0; P256_PKCS8_CAPACITY];
    let n = Provider.generate_p256(&mut raw).unwrap();
    let root = Provider.load_p256(&raw[..n]).unwrap();
    let ca = pem(
        "CERTIFICATE",
        &certificate_names(&root, &root, true, 1, &["localhost"], false),
    );
    let n = Provider.generate_p256(&mut raw).unwrap();
    let leaf = Provider.load_p256(&raw[..n]).unwrap();
    let certificate = pem(
        "CERTIFICATE",
        &certificate_names(&leaf, &root, false, 2, &["localhost"], false),
    );
    (
        [certificate, ca.clone()].concat(),
        pem("PRIVATE KEY", &raw[..n]),
        ca,
    )
}

fn large_material() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    large_material_names(&["localhost"])
}

pub(super) fn large_material_names(names: &[&str]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    large_material_padding(names, 15_000)
}

pub(super) fn large_routing_material(names: &[&str]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let name_bytes: usize = names.iter().map(|name| name.len() + 4).sum();
    large_material_padding(names, 15_000usize.checked_sub(name_bytes).unwrap())
}

fn large_material_padding(names: &[&str], leaf_padding: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut raw = [0; P256_PKCS8_CAPACITY];
    let n = Provider.generate_p256(&mut raw).unwrap();
    let root = Provider.load_p256(&raw[..n]).unwrap();
    let n = Provider.generate_p256(&mut raw).unwrap();
    let issuer = Provider.load_p256(&raw[..n]).unwrap();
    let n = Provider.generate_p256(&mut raw).unwrap();
    let leaf = Provider.load_p256(&raw[..n]).unwrap();
    let key = pem("PRIVATE KEY", &raw[..n]);
    let mut chain = Vec::new();
    let mut ca = Vec::new();
    for (key, signer, ca_flag, serial, subject, issuer_name) in [
        (
            &leaf,
            &issuer,
            false,
            3,
            b"localhost".as_slice(),
            b"local-test-issuer".as_slice(),
        ),
        (
            &issuer,
            &root,
            true,
            2,
            b"local-test-issuer".as_slice(),
            b"local-test-root".as_slice(),
        ),
        (
            &root,
            &root,
            true,
            1,
            b"local-test-root".as_slice(),
            b"local-test-root".as_slice(),
        ),
    ] {
        let der = certificate_with(
            key,
            signer,
            &Certificate {
                ca: ca_flag,
                serial,
                names: if ca_flag { &["localhost"] } else { names },
                client: false,
                issuer: issuer_name,
                subject,
                padding: if ca_flag { 15_000 } else { leaf_padding },
            },
        );
        assert!(der.len() > 15_000 && der.len() <= 16 * 1024);
        let encoded = pem("CERTIFICATE", &der);
        chain.extend_from_slice(&encoded);
        if serial == 1 {
            ca = encoded;
        }
    }
    assert!(chain.len() > 60 * 1024 && chain.len() <= 64 * 1024);
    (chain, key, ca)
}
fn transfer(from: &mut TlsConnection<Buffer>, to: &mut TlsConnection<Buffer>, byte: u8) {
    let input = [byte; 16_384];
    let mut output = [0; 16_384];
    let (mut sent, mut received) = (0, 0);
    for _ in 0..100_000 {
        if sent < input.len() {
            match from.write(black_box(&input[sent..])).unwrap() {
                IoProgress::Bytes(n) => sent += n,
                IoProgress::Pending => {}
                IoProgress::Closed => panic!("writer closed"),
            }
        }
        let flushed = from.flush().unwrap() == FlushProgress::Complete;
        if received < output.len() {
            match to.read(&mut output[received..]).unwrap() {
                IoProgress::Bytes(n) => received += n,
                IoProgress::Pending => {}
                IoProgress::Closed => panic!("reader closed"),
            }
        }
        if sent == input.len() && received == output.len() && flushed {
            assert_eq!(black_box(output), input);
            return;
        }
        std::thread::yield_now();
    }
    panic!("local record transfer did not complete");
}

pub fn run(mut observe: impl FnMut()) {
    run_profile(false, &mut observe);
}

pub fn run_large(mut observe: impl FnMut()) {
    run_profile(true, &mut observe);
}

fn run_profile(large: bool, mut observe: impl FnMut()) {
    observe();
    {
        let (chain, key, ca) = if large { large_material() } else { material() };
        observe();
        let mut scratch = vec![0; stream::SCRATCH_BYTES];
        let loaded = load::read(
            storage::Storage::try_new().unwrap(),
            &mut stanza::Pending::new(),
            &mut scratch,
            &mut Cursor::new(SOURCE),
        )
        .unwrap();
        let config = materialize::read_text(loaded, &mut scratch, |_| {
            Ok::<_, Error>(Cursor::new(b"password\n"))
        })
        .unwrap();
        drop(scratch);
        let source = Arc::new(FixedClock);
        let clock = Arc::new(ClockHandle::new(TlsClockSource::new(source.clone())));
        let mut set = GenerationSet::at_startup();
        observe();
        let prepared = TlsPolicies::prepare(set.reserve().unwrap(), &config, clock, |request| {
            let bytes = match request.kind() {
                MaterialKind::Chain => &chain,
                MaterialKind::Key => &key,
                MaterialKind::RelayCa => &ca,
                _ => panic!("unexpected material role"),
            };
            Ok::<_, Error>(Cursor::new(bytes.as_slice()))
        })
        .unwrap();
        drop(set.publish(prepared).unwrap());
        let lease = set.current().unwrap();
        let relay = TlsPolicies::relay(&lease).unwrap();
        let smtp = TlsPolicies::listener(&lease, "smtp").unwrap();
        observe();
        let pool = HandshakePool::new(2).unwrap();
        let (ci, co, si, so) = (buffer(), buffer(), buffer(), buffer());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        drop(listener);
        let client = TcpTransport::from_stream(client).unwrap();
        let server = TcpTransport::from_stream(server).unwrap();
        observe();
        let cap = Deadline::after(Tick(0), 1000).unwrap();
        let mut client = TlsPolicies::reserve_session(lease.clone(), relay, &pool, ci, co)
            .unwrap()
            .construct()
            .unwrap()
            .handoff(client, &[], source.clone(), cap, cap)
            .unwrap();
        let mut server = TlsPolicies::reserve_session(lease, smtp, &pool, si, so)
            .unwrap()
            .construct()
            .unwrap()
            .handoff(server, &[], source, cap, cap)
            .unwrap();
        assert_eq!(pool.available(), 0);
        observe();
        let mut complete = false;
        for _ in 0..100_000 {
            if matches!(
                (
                    client.handshake(cap).unwrap(),
                    server.handshake(cap).unwrap()
                ),
                (Handshake::Complete(_), Handshake::Complete(_))
            ) {
                complete = true;
                break;
            }
            std::thread::yield_now();
        }
        assert!(complete);
        assert_eq!(pool.available(), 2);
        let info = client.info().unwrap();
        assert_eq!(info.version, TlsVersion::V13);
        assert_eq!(info.peer, PeerVerification::ServerName);
        assert_eq!(server.info().unwrap().peer, PeerVerification::None);
        observe();
        transfer(&mut client, &mut server, 0x31);
        transfer(&mut server, &mut client, 0x42);
        observe();
        for byte in 0..32 {
            transfer(&mut client, &mut server, 0x60 + byte);
            transfer(&mut server, &mut client, 0xa0 + byte);
        }
        observe();
        let client_buffers = client.into_buffers().unwrap();
        observe();
        let server_buffers = server.into_buffers().unwrap();
        observe();
        black_box((&client_buffers, &server_buffers));
        drop(client_buffers);
        drop(server_buffers);
        observe();
    }
    observe();
}

/// Many distinct subjects keep substantial anchor data after DER is discarded.
pub(crate) fn large_trust_bundle() -> Vec<u8> {
    let mut raw = [0; P256_PKCS8_CAPACITY];
    let n = Provider.generate_p256(&mut raw).unwrap();
    let key = Provider.load_p256(&raw[..n]).unwrap();
    let mut bundle = Vec::new();
    for serial in 1..=128 {
        let subject = format!("{}-{serial:03}", "r".repeat(56));
        let cert = certificate_with(
            &key,
            &key,
            &Certificate {
                ca: true,
                // Keep a one-byte DER serial positive; issuers differ.
                serial: if serial == 128 { 1 } else { serial },
                names: &[],
                client: false,
                issuer: subject.as_bytes(),
                subject: subject.as_bytes(),
                padding: 220,
            },
        );
        bundle.extend_from_slice(&pem("CERTIFICATE", &cert));
    }
    assert!(bundle.len() > 112 * 1024);
    assert!(bundle.len() <= td_mta::config::inputs::MAX_CA_BYTES);
    let trust = td_crypto::TrustStore::from_pem(&bundle).unwrap();
    assert_eq!(trust.anchor_count(), 128);
    bundle
}
