//! The controller owns the peer process and material; this process never forks.
#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use std::{
    hint::black_box,
    io::{Cursor, Read},
    net::{SocketAddr, TcpStream},
    sync::Arc,
    time::{Duration, Instant},
};
use td_crypto::{ClockHandle, TlsPhase};
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

pub const PHASES: [&str; 11] = [
    "baseline",
    "config",
    "generation",
    "buffers",
    "constructed",
    "handshake",
    "record",
    "repeated",
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
chain_file = "/unused-chain"
key_file = "/unused-key"
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
pub fn label() -> &'static str {
    let large = match std::env::var("TD_MTA_TEST_PEER_LARGE_TICKET").as_deref() {
        Ok("1") => true,
        Ok("0") => false,
        _ => panic!("invalid fixture ticket mode"),
    };
    match (
        std::env::var("TD_MTA_TEST_PEER_VERSION").unwrap().as_str(),
        large,
    ) {
        ("1.2", false) => "remote12",
        ("1.3", false) => "remote13",
        ("1.3", true) => "remote13large",
        _ => panic!("invalid fixture version/ticket combination"),
    }
}
fn exchange(client: &mut TlsConnection<Buffer>, value: u8) {
    let input = [value; 16_384];
    let mut output = [0; 16_384];
    let (mut sent, mut received) = (0, 0);
    let start = Instant::now();
    loop {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "remote record timed out"
        );
        if sent < input.len() {
            match client.write(black_box(&input[sent..])).unwrap() {
                IoProgress::Bytes(n) => sent += n,
                IoProgress::Pending => {}
                IoProgress::Closed => panic!("remote writer closed"),
            }
        }
        let flushed = client.flush().unwrap() == FlushProgress::Complete;
        if received < output.len() {
            match client.read(&mut output[received..]).unwrap() {
                IoProgress::Bytes(n) => received += n,
                IoProgress::Pending => {}
                IoProgress::Closed => panic!("remote reader closed"),
            }
        }
        if sent == input.len() && received == output.len() && flushed {
            assert_eq!(black_box(output), input);
            return;
        }
        std::thread::yield_now();
    }
}

pub fn run(mut observe: impl FnMut()) {
    observe();
    {
        let expected = if label() == "remote12" {
            TlsVersion::V12
        } else {
            TlsVersion::V13
        };
        let address: SocketAddr = std::env::var("TD_MTA_TEST_PEER_ADDRESS")
            .unwrap()
            .parse()
            .unwrap();
        assert!(address.ip().is_loopback());
        let directory =
            std::path::PathBuf::from(std::env::var_os("TD_MTA_TEST_PEER_MATERIAL").unwrap());
        let mut ca = Vec::new();
        std::fs::File::open(directory.join("root.pem"))
            .unwrap()
            .take(131_073)
            .read_to_end(&mut ca)
            .unwrap();
        assert!(ca.len() <= 131_072);
        drop(directory);
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
        let prepared =
            TlsPolicies::prepare_clients(set.reserve().unwrap(), &config, clock, |request| {
                assert_eq!(request.kind(), MaterialKind::RelayCa);
                Ok::<_, Error>(Cursor::new(ca.as_slice()))
            })
            .unwrap();
        drop(set.publish(prepared).unwrap());
        let lease = set.current().unwrap();
        let relay = TlsPolicies::relay(&lease).unwrap();
        let pool = HandshakePool::new(1).unwrap();
        observe();
        let (input, output) = (buffer(), buffer());
        let socket = TcpStream::connect_timeout(&address, Duration::from_secs(3)).unwrap();
        let socket = TcpTransport::from_stream(socket).unwrap();
        observe();
        let cap = Deadline::after(Tick(0), 1000).unwrap();
        let reserved = TlsPolicies::reserve_session(lease, relay, &pool, input, output)
            .unwrap()
            .construct()
            .unwrap();
        assert_eq!(reserved.status().phase, TlsPhase::Handshaking);
        let mut client = reserved.handoff(socket, &[], source, cap, cap).unwrap();
        observe();
        let start = Instant::now();
        loop {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "remote handshake timed out"
            );
            if matches!(client.handshake(cap).unwrap(), Handshake::Complete(_)) {
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(pool.available(), 1);
        let info = client.info().unwrap();
        assert_eq!(info.version, expected);
        assert_eq!(info.peer, PeerVerification::ServerName);
        observe();
        exchange(&mut client, 0);
        observe();
        for value in 1..=32 {
            exchange(&mut client, value);
        }
        observe();
        let buffers = client.into_buffers().unwrap();
        observe();
        black_box(&buffers);
        drop(buffers);
        observe();
        black_box((&ca, &config, &set));
    }
    observe();
}
