use super::*;
use crate::{
    ports::{
        Deadline, FlushProgress, Handshake, IoProgress, PeerVerification, TlsTransport, Transport,
    },
    transport::TcpTransport,
};
use std::{
    net::TcpListener,
    os::unix::fs::DirBuilderExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::atomic::AtomicUsize,
    time::{Duration, Instant},
};
use td_crypto::Digest;

struct Peer(Child);
impl Drop for Peer {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}
struct Directory(PathBuf);
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn material() -> (Material, Vec<u8>, Vec<u8>, [u8; 32]) {
    let mut root = [0; P256_PKCS8_CAPACITY];
    let mut server = [0; P256_PKCS8_CAPACITY];
    let mut client = [0; P256_PKCS8_CAPACITY];
    let r = Provider.generate_p256(&mut root).unwrap();
    let s = Provider.generate_p256(&mut server).unwrap();
    let c = Provider.generate_p256(&mut client).unwrap();
    let root_key = Provider.load_p256(&root[..r]).unwrap();
    let server_key = Provider.load_p256(&server[..s]).unwrap();
    let client_key = Provider.load_p256(&client[..c]).unwrap();
    let ca = pem("CERTIFICATE", &certificate(&root_key, &root_key, true));
    let chain = [
        pem("CERTIFICATE", &certificate(&server_key, &root_key, false)),
        ca.clone(),
    ]
    .concat();
    let leaf = crate::tls_io::tests::client_certificate(&client_key, &root_key);
    let mut hash = td_crypto::Sha256::try_new().unwrap();
    hash.update(&leaf).unwrap();
    let pin = hash.finish().unwrap();
    let client_chain = [pem("CERTIFICATE", &leaf), ca.clone()].concat();
    let source = Arc::new(SwitchClock(AtomicBool::new(false)));
    (
        Material {
            chain,
            key: pem("PRIVATE KEY", &server[..s]),
            ca,
            clock: Arc::new(ClockHandle::new(TlsClockSource::new(source.clone()))),
            source,
        },
        client_chain,
        pem("PRIVATE KEY", &client[..c]),
        pin,
    )
}

fn exercise(version: &str, next: bool, bad_pin: bool, bad_peer: bool) {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let _serial = serial();
    let (material, client_chain, client_key, pin) = material();
    let hex: String = pin.iter().map(|byte| format!("{byte:02x}")).collect();
    let mut config = source(true);
    if next {
        config = config.replace(
            "[gateway_peer",
            &format!("next_client_cert_sha256 = \"{hex}\"\n[gateway_peer"),
        );
    } else if !bad_pin {
        config = config.replace(&"11".repeat(32), &hex);
    }
    if !bad_peer {
        config = config.replace("192.0.2.0/24", "127.0.0.0/8");
    }
    let lease = material.published(&config);
    let id = TlsPolicies::listener(&lease, "gateway").unwrap();
    let pool = HandshakePool::new(1).unwrap();
    let (input, output) = buffers();
    let reservation = TlsPolicies::reserve_session(lease, id, &pool, input, output)
        .unwrap()
        .construct()
        .unwrap();
    let directory = Directory(std::env::temp_dir().join(format!(
        "td-mta-gateway-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&directory.0)
        .unwrap();
    for (name, bytes) in [
        ("root.pem", &material.ca),
        ("client.pem", &client_chain),
        ("client-key.pem", &client_key),
    ] {
        std::fs::write(directory.0.join(name), bytes).unwrap();
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let executable = std::env::var_os("TD_MTA_TEST_TLS_PEER")
        .unwrap_or_else(|| panic!("run mail tests through td-builder crypto-cargo"));
    let mut peer = Peer(
        Command::new(executable)
            .args([
                "--exact",
                "tls_peer_tests::gateway_client_process",
                "--ignored",
                "--test-threads=1",
            ])
            .env_clear()
            .env(
                "TD_MTA_TEST_PEER_ADDRESS",
                listener.local_addr().unwrap().to_string(),
            )
            .env("TD_MTA_TEST_PEER_MATERIAL", &directory.0)
            .env("TD_MTA_TEST_PEER_VERSION", version)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let start = Instant::now();
    let socket = loop {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "peer never connected"
        );
        match listener.accept() {
            Ok((socket, _)) => break socket,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                assert!(
                    peer.0.try_wait().unwrap().is_none(),
                    "peer exited before connecting"
                );
                std::thread::yield_now();
            }
            Err(e) => panic!("accept peer: {e}"),
        }
    };
    let cap = Deadline::after(Tick(0), 1000).unwrap();
    let mut connection = reservation
        .handoff(
            TcpTransport::from_stream(socket).unwrap(),
            &[],
            material.source.clone(),
            cap,
            cap,
        )
        .unwrap();
    let handshake = loop {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "handshake stalled"
        );
        match connection.handshake(cap) {
            Ok(Handshake::Pending) => std::thread::yield_now(),
            result => break result,
        }
    };
    assert_eq!(pool.available(), 1);
    if bad_pin || bad_peer {
        assert_eq!(handshake, Err(Error::Forbidden));
        assert_eq!(connection.info(), None);
        assert_eq!(connection.write(b"250 OK\r\n"), Err(Error::Forbidden));
    } else {
        let info = connection.info().unwrap();
        assert_eq!(handshake, Ok(Handshake::Complete(info)));
        assert_eq!(info.peer, PeerVerification::Gateway(pin));
        assert_eq!(
            info.version,
            if version == "1.2" {
                crate::ports::TlsVersion::V12
            } else {
                crate::ports::TlsVersion::V13
            }
        );
        let mut received = Vec::new();
        while received.len() < 14 {
            assert!(start.elapsed() < Duration::from_secs(5));
            let mut chunk = [0; 14];
            match connection.read(&mut chunk).unwrap() {
                IoProgress::Bytes(n) => received.extend_from_slice(&chunk[..n]),
                IoProgress::Pending => std::thread::yield_now(),
                IoProgress::Closed => panic!("peer closed before EHLO"),
            }
        }
        assert_eq!(received, b"EHLO gateway\r\n");
        let mut sent = 0;
        while sent < 8 {
            assert!(start.elapsed() < Duration::from_secs(5));
            if let IoProgress::Bytes(n) = connection.write(&b"250 OK\r\n"[sent..]).unwrap() {
                sent += n;
            }
        }
        while connection.flush().unwrap() != FlushProgress::Complete {
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::yield_now();
        }
    }
    let status = loop {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "peer did not exit"
        );
        if let Some(status) = peer.0.try_wait().unwrap() {
            break status;
        }
        std::thread::yield_now();
    };
    assert_eq!(status.success(), !(bad_pin || bad_peer));
    drop(connection.into_buffers().unwrap());
}

#[test]
fn gateway_mutual_tls_accepts_current_and_next_verified_leaf_pins() {
    for version in ["1.2", "1.3"] {
        exercise(version, false, false, false);
        exercise(version, true, false, false);
    }
}
#[test]
fn gateway_mutual_tls_refuses_verified_leaf_with_wrong_pin_or_actual_peer() {
    for version in ["1.2", "1.3"] {
        exercise(version, false, true, false);
        exercise(version, false, false, true);
    }
}
