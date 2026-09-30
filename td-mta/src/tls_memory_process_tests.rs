//! Unwrapped controller; instrumented children never create a process.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use crate::tls_io::tests::certificate_fixture::{certificate_with, pem, Certificate};
use std::{
    io::Read,
    os::unix::fs::DirBuilderExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use td_crypto::{Crypto, Provider, P256_PKCS8_CAPACITY};

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
fn material(directory: &Directory) {
    let mut keys = Vec::new();
    for i in 0..4 {
        let mut raw = [0; P256_PKCS8_CAPACITY];
        let n = Provider.generate_p256(&mut raw).unwrap();
        keys.push(Provider.load_p256(&raw[..n]).unwrap());
        if i == 0 {
            std::fs::write(directory.0.join("server-key.der"), &raw[..n]).unwrap();
        }
    }
    let names: [&[u8]; 4] = [
        b"localhost",
        b"local-test-intermediate1",
        b"local-test-intermediate2",
        b"local-test-root",
    ];
    let mut total = 0;
    for i in 0..4 {
        let issuer = (i + 1).min(3);
        let der = certificate_with(
            &keys[i],
            &keys[issuer],
            &Certificate {
                ca: i != 0,
                serial: i as u8 + 1,
                names: &["localhost"],
                client: false,
                issuer: names[issuer],
                subject: names[i],
                padding: 15_800,
            },
        );
        assert!(der.len() <= 16 * 1024);
        total += der.len();
        std::fs::write(directory.0.join(format!("cert{i}.der")), &der).unwrap();
        if i == 3 {
            std::fs::write(directory.0.join("root.pem"), pem("CERTIFICATE", &der)).unwrap();
        }
    }
    assert!(total > 63 * 1024 && total <= 65_000);
}
fn child_log(log: &std::path::Path) -> String {
    let mut detail = String::new();
    std::fs::File::open(log)
        .unwrap()
        .take(8192)
        .read_to_string(&mut detail)
        .unwrap();
    detail
}
fn child_failure(status: std::process::ExitStatus, log: &std::path::Path) -> ! {
    panic!("fixture child failed: {status}\n{}", child_log(log));
}
fn wait(peer: &mut Peer, start: Instant, log: &std::path::Path) {
    loop {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "fixture child timed out\n{}",
            child_log(log)
        );
        if let Some(status) = peer.0.try_wait().unwrap() {
            if !status.success() {
                child_failure(status, log);
            }
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
#[ignore = "invoked by the isolated runner with explicit peer and observer paths"]
fn remote_chain_observations() {
    let domain = std::env::var("TD_MTA_TEST_MEMORY_DOMAIN").unwrap();
    assert!(matches!(domain.as_str(), "rust" | "native" | "rss"));
    let version = std::env::var("TD_MTA_TEST_PEER_VERSION").unwrap();
    assert!(matches!(version.as_str(), "1.2" | "1.3"));
    let path = std::env::temp_dir().join(format!(
        "td-mta-remote-chain-{}-{domain}-{version}",
        std::process::id()
    ));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&path)
        .unwrap();
    let directory = Directory(path);
    material(&directory);
    let peer_log = std::fs::File::create(directory.0.join("peer.log")).unwrap();
    let mut peer = Peer(
        Command::new(std::env::var_os("TD_MTA_TEST_TLS_PEER").unwrap())
            .args([
                "--exact",
                "tls_peer_tests::large_chain_server_process",
                "--ignored",
                "--test-threads=1",
            ])
            .env_clear()
            .env("TD_MTA_TEST_PEER_MATERIAL", &directory.0)
            .env("TD_MTA_TEST_PEER_VERSION", &version)
            .stdin(Stdio::null())
            .stdout(peer_log.try_clone().unwrap())
            .stderr(peer_log)
            .spawn()
            .unwrap(),
    );
    let start = Instant::now();
    let address = loop {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "peer never became ready\n{}",
            child_log(&directory.0.join("peer.log"))
        );
        if let Some(status) = peer.0.try_wait().unwrap() {
            child_failure(status, &directory.0.join("peer.log"));
        }
        match std::fs::read_to_string(directory.0.join("address")) {
            Ok(address) if address.parse::<std::net::SocketAddr>().is_ok() => break address,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => panic!("read peer address: {e}"),
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    let output = directory.0.join("observer.log");
    let output_file = std::fs::File::create(&output).unwrap();
    let mut observer = Peer(
        Command::new(std::env::var_os("TD_MTA_TEST_MEMORY_PROBE").unwrap())
            .arg("--tls-remote-chain")
            .env_clear()
            .env("TD_MTA_TEST_PEER_ADDRESS", address)
            .env("TD_MTA_TEST_PEER_MATERIAL", &directory.0)
            .env("TD_MTA_TEST_PEER_VERSION", &version)
            .stdin(Stdio::null())
            .stdout(output_file.try_clone().unwrap())
            .stderr(output_file)
            .spawn()
            .unwrap(),
    );
    wait(&mut observer, start, &output);
    wait(&mut peer, start, &directory.0.join("peer.log"));
    let mut text = String::new();
    std::fs::File::open(&output)
        .unwrap()
        .take(8193)
        .read_to_string(&mut text)
        .unwrap();
    assert!(text.len() <= 8192);
    println!("\nremote-chain-output-begin\n{text}remote-chain-output-end");
    println!("remote-chain-controller-v1: {domain} {version} passed");
}
