#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! HTTPS through the real binary. td-net links no built-in crypto provider, so an
//! agent that names no TLS configuration, as http.rs's do, depends on main having
//! installed td's: without that ureq panics at the first connection, which no
//! plain-HTTP test or in-process unit test can see.

#[path = "../../td-fs/src/temp_dir.rs"]
mod temp_dir;

use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const TD_NET: &str = env!("CARGO_BIN_EXE_td-net");

struct Origin(Child);

impl Drop for Origin {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A minted identity serving `DIR/root/page.txt`, listening when this returns.
fn origin(dir: &std::path::Path) -> (Origin, u16) {
    let mint = Command::new(TD_NET)
        .args(["test-https", "mint"])
        .arg(dir)
        .output()
        .unwrap();
    assert!(mint.status.success(), "{mint:?}");
    let root = dir.join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("page.txt"), "served over td's own TLS\n").unwrap();
    let port = free_port();
    let child = Command::new(TD_NET)
        .args(["test-https", "serve", "--listen"])
        .arg(format!("127.0.0.1:{port}"))
        .arg("--cert")
        .arg(dir.join("server.pem"))
        .arg("--key")
        .arg(dir.join("server.key"))
        .arg("--root")
        .arg(&root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let origin = Origin(child);
    let deadline = Instant::now() + Duration::from_secs(20);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "the origin never listened");
        std::thread::sleep(Duration::from_millis(20));
    }
    (origin, port)
}

#[test]
fn an_agent_with_no_tls_configuration_reaches_a_certificate_check_rather_than_a_panic() {
    let dir = temp_dir::TempDir::new("td-tls-default").unwrap();
    let (_origin, port) = origin(dir.path());
    let out = Command::new(TD_NET)
        .args(["fetch", "fetch"])
        .arg(format!("https://localhost:{port}/page.txt"))
        .arg("0".repeat(64))
        .arg(dir.path().join("fetched"))
        .env_remove("TD_FEED_BASE")
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{said}");
    assert!(!said.contains("panicked"), "{said}");
    // The minted authority is in no public root store: td-fetch must say so.
    assert!(said.contains("UnknownIssuer"), "{said}");
}

#[test]
fn the_probe_verifies_a_served_identity_end_to_end() {
    let dir = temp_dir::TempDir::new("td-tls-probe").unwrap();
    let (_origin, port) = origin(dir.path());
    let probe = |ca: &std::path::Path, name: &str| {
        Command::new(TD_NET)
            .args(["test-https", "probe", "--connect"])
            .arg(format!("127.0.0.1:{port}"))
            .args(["--server-name", name, "--ca"])
            .arg(ca)
            .args(["--path", "/page.txt", "--expect", "td's own TLS"])
            .output()
            .unwrap()
    };
    let ca = dir.path().join("ca.pem");
    let good = probe(&ca, "localhost");
    assert!(good.status.success(), "{good:?}");
    assert!(!probe(&ca, "elsewhere").status.success(), "another name");
    // The server's own certificate is no authority.
    let leaf = dir.path().join("server.pem");
    assert!(!probe(&leaf, "localhost").status.success(), "another root");
}
