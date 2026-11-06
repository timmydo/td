#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! td-curl end to end: the built td-net linked as `curl`, against td-test-https's
//! Git origin over real TLS on loopback. The fixture's plain-text paths answer
//! without a repository.

#[path = "../../td-fs/src/temp_dir.rs"]
mod temp_dir;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TD_NET: &str = env!("CARGO_BIN_EXE_td-net");
const BODY: &str = "0123 refs/heads/main\n";

struct Site {
    dir: temp_dir::TempDir,
}

impl Site {
    fn new(tag: &str) -> Self {
        let dir = temp_dir::TempDir::new(&format!("td-curl-{tag}")).unwrap();
        for name in ["curl", "td-curl", "td-test-https"] {
            symlink(TD_NET, dir.path().join(name)).unwrap();
        }
        std::fs::create_dir(dir.path().join("mint")).unwrap();
        std::fs::create_dir(dir.path().join("root")).unwrap();
        let minted = Command::new(dir.path().join("td-test-https"))
            .arg("mint")
            .arg(dir.path().join("mint"))
            .status()
            .unwrap();
        assert!(minted.success());
        Self { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn untrusting(&self) -> Command {
        let mut command = Command::new(self.path("curl"));
        command.env_clear().current_dir(self.dir.path());
        command
    }

    fn curl(&self) -> Command {
        let mut command = self.untrusting();
        command.arg("--cacert").arg(self.path("mint/ca.pem"));
        command
    }

    fn serve(&self, extra: &[&str]) -> Origin {
        self.serve_on("127.0.0.1:0", extra)
    }

    fn serve_on(&self, listen: &str, extra: &[&str]) -> Origin {
        let mut child = Command::new(self.path("td-test-https"))
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .arg("serve-git")
            .args(["--listen", listen, "--cert"])
            .arg(self.path("mint/server.pem"))
            .arg("--key")
            .arg(self.path("mint/server.key"))
            .arg("--root")
            .arg(self.path("root"))
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let bound = line.trim().strip_prefix("listening ").unwrap().to_string();
        let port = bound.rsplit(':').next().unwrap().to_string();
        Origin { child, port }
    }
}

struct Origin {
    child: Child,
    port: String,
}

impl Origin {
    fn url(&self, path: &str) -> String {
        format!("https://localhost:{}/{path}", self.port)
    }
}

impl Drop for Origin {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn run(command: &mut Command, args: &[&str]) -> Output {
    command.args(args).output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[track_caller]
fn expect(output: &Output, code: i32, text: &str) {
    assert_eq!(output.status.code(), Some(code), "{}", stderr(output));
    assert!(
        stderr(output).contains(text),
        "wanted {text:?} in: {}",
        stderr(output)
    );
}

#[test]
fn it_never_passes_for_curl() {
    let site = Site::new("name");
    for name in ["curl", "td-curl"] {
        let version = Command::new(site.path(name))
            .arg("--version")
            .output()
            .unwrap();
        assert!(version.status.success());
        assert!(stdout(&version).starts_with("td-curl "), "{version:?}");
        assert!(stdout(&version).contains("not curl"), "{version:?}");
        let refused = Command::new(site.path(name))
            .args(["--insecure", "https://localhost:9/"])
            .output()
            .unwrap();
        expect(
            &refused,
            2,
            "td-curl (not curl): option --insecure is not supported",
        );
        // A failure at run time says so as well.
        let down = Command::new(site.path(name))
            .args(["-f", "http://127.0.0.1:9/"])
            .env_clear()
            .output()
            .unwrap();
        expect(&down, 1, "td-curl (not curl): (1)");
        // Not text: refused by name, not a panic.
        let bytes = Command::new(site.path(name))
            .arg(std::ffi::OsStr::from_bytes(b"caf\xe9"))
            .output()
            .unwrap();
        expect(&bytes, 2, "td-curl (not curl): an argument is not UTF-8");
    }
    let help = run(&mut site.untrusting(), &["--help"]);
    assert!(stdout(&help).contains("It is not curl"), "{help:?}");
}

#[test]
fn a_download_reaches_where_it_was_asked_to() {
    let site = Site::new("download");
    let log = site.path("origin.log");
    let origin = site.serve(&["--log", log.to_str().unwrap()]);
    let url = origin.url("dumb/file.txt");

    let output = run(&mut site.curl(), &["-fsSL", &url]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), BODY);
    assert_eq!(stderr(&output), "");
    let heard = std::fs::read_to_string(&log).unwrap();
    assert!(heard.contains("GET /dumb/file.txt? auth=false"), "{heard}");
    assert!(heard.contains("agent=td-curl/"), "{heard}");
    assert!(heard.contains("accept=*/* bytes=0"), "{heard}");

    let output = run(&mut site.curl(), &["-o", "out", &url]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
    assert_eq!(std::fs::read_to_string(site.path("out")).unwrap(), BODY);

    let output = run(&mut site.curl(), &["-O", &format!("{url}?v=1#top")]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        std::fs::read_to_string(site.path("file.txt")).unwrap(),
        BODY
    );

    let output = run(&mut site.curl(), &["-O", &origin.url("dumb/")]);
    expect(&output, 23, "names no file");

    // Agreed spellings of "over TLS 1.2 or better, to nothing but https".
    let output = run(&mut site.curl(), &["--proto", "=https", "--tlsv1.2", &url]);
    assert!(output.status.success(), "{}", stderr(&output));
    let output = run(&mut site.curl(), &["-A", "someone", "-o", "-", &url]);
    assert_eq!(stdout(&output), BODY);
    let heard = std::fs::read_to_string(&log).unwrap();
    assert!(heard.contains("agent=someone"), "{heard}");

    // A header named replaces what would have gone, rather than joining it.
    let output = run(&mut site.curl(), &["-H", "Accept: x/y", &url]);
    assert!(output.status.success(), "{}", stderr(&output));
    let heard = std::fs::read_to_string(&log).unwrap();
    assert!(heard.contains("accept=x/y bytes=0"), "{heard}");

    let output = run(&mut site.curl(), &["-A", "a\nb", &url]);
    expect(&output, 2, "control character");

    let output = run(&mut site.curl(), &["-o", "/dev/full", &url]);
    expect(&output, 23, "/dev/full");
}

#[test]
fn an_http_error_is_reported_only_when_asked_and_leaves_a_file_alone() {
    let site = Site::new("errors");
    let origin = site.serve(&[]);
    let url = origin.url("forbidden/x");

    // As curl: without -f, an error page is a response like any other.
    let output = run(&mut site.curl(), &[&url]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "the fixture forbids this\n");

    let output = run(&mut site.curl(), &["-f", &url]);
    expect(
        &output,
        22,
        "td-curl (not curl): (22) The requested URL returned error: 403",
    );
    assert_eq!(stdout(&output), "");
    let output = run(&mut site.curl(), &["-fs", &url]);
    assert_eq!(output.status.code(), Some(22));
    assert_eq!(stderr(&output), "");
    let output = run(&mut site.curl(), &["-fsS", &url]);
    expect(&output, 22, "returned error: 403");

    std::fs::write(site.path("kept"), "from before\n").unwrap();
    let output = run(&mut site.curl(), &["-f", "-o", "kept", &url]);
    assert_eq!(output.status.code(), Some(22));
    assert_eq!(
        std::fs::read_to_string(site.path("kept")).unwrap(),
        "from before\n"
    );

    let output = run(
        &mut site.curl(),
        &["-o", "/no/such/dir/x", &origin.url("dumb/x")],
    );
    expect(&output, 23, "/no/such/dir/x");
}

#[test]
fn a_body_that_may_be_cut_short_is_a_failure() {
    let site = Site::new("short");
    let origin = site.serve(&[]);
    for path in ["short/x", "short-chunks/x"] {
        let output = run(&mut site.curl(), &["-fsS", "-o", "out", &origin.url(path)]);
        expect(&output, 1, "the transfer did not finish");
    }
    // No length at all: nothing would show a cut, so nothing is handed over.
    std::fs::remove_file(site.path("out")).unwrap();
    let output = run(
        &mut site.curl(),
        &["-fsS", "-o", "out", &origin.url("unframed/x")],
    );
    expect(&output, 1, "did not say how long");
    assert!(!site.path("out").exists());
    for path in ["unframed/x", "odd-encoding/x"] {
        let output = run(&mut site.curl(), &[&origin.url(path)]);
        expect(&output, 1, "did not say how long");
        assert_eq!(stdout(&output), "");
    }
}

#[test]
fn trust_is_the_named_bundle_and_never_optional() {
    let site = Site::new("trust");
    let origin = site.serve_on("0.0.0.0:0", &[]);
    let url = origin.url("dumb/x");
    let ca = site.path("mint/ca.pem");

    // No flag, no variable: the system's store, which knows nothing of this site.
    let output = run(&mut site.untrusting(), &[&url]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
    let said = stderr(&output);
    assert!(
        said.contains("invalid peer certificate") || said.contains("CA bundle"),
        "{said}"
    );

    for variable in ["CURL_CA_BUNDLE", "SSL_CERT_FILE"] {
        let output = run(site.untrusting().env(variable, &ca), &[&url]);
        assert!(output.status.success(), "{variable}: {}", stderr(&output));
    }
    let output = run(
        site.untrusting().env("SSL_CERT_FILE", "/no/such/file"),
        &[&url],
    );
    expect(&output, 1, "CA bundle /no/such/file");

    let other = site.path("other");
    std::fs::create_dir(&other).unwrap();
    assert!(Command::new(site.path("td-test-https"))
        .arg("mint")
        .arg(&other)
        .status()
        .unwrap()
        .success());
    let output = run(
        site.untrusting().arg("--cacert").arg(other.join("ca.pem")),
        &[&url],
    );
    expect(&output, 1, "invalid peer certificate");

    // The right authority, the wrong name.
    let wrong = format!("https://127.0.0.2:{}/dumb/x", origin.port);
    let output = run(&mut site.curl(), &[&wrong]);
    expect(&output, 1, "invalid peer certificate");

    let output = run(
        site.untrusting().env("SSL_CERT_DIR", "/etc/ssl/certs"),
        &[&url],
    );
    expect(&output, 1, "SSL_CERT_DIR is set");
    let output = run(site.curl().env("SSL_CERT_DIR", "/etc/ssl/certs"), &[&url]);
    assert!(output.status.success(), "{}", stderr(&output));

    for refused in ["-k", "--insecure", "--no-verify", "--cert"] {
        let output = run(&mut site.curl(), &[refused, &url]);
        assert_eq!(output.status.code(), Some(2), "{refused}");
    }
}

#[test]
fn redirects_are_asked_for_keep_headers_home_and_never_downgrade() {
    let site = Site::new("redirect");
    let far_log = site.path("far.log");
    let far = site.serve(&["--log", far_log.to_str().unwrap()]);
    // The same host on another port is another origin.
    let elsewhere = format!("https://localhost:{}", far.port);
    let near_log = site.path("near.log");
    let near = site.serve(&[
        "--elsewhere",
        &elsewhere,
        "--log",
        near_log.to_str().unwrap(),
    ]);

    // Not asked: the redirect itself is the answer.
    let moved = near.url("moved/dumb/x");
    let output = run(&mut site.curl(), &["-f", &moved]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");

    let header = "Authorization: Basic c2VjcmV0";
    let output = run(&mut site.curl(), &["-fL", "-H", header, &moved]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), BODY);
    let heard = std::fs::read_to_string(&near_log).unwrap();
    assert!(heard.contains("GET /dumb/x? auth=true"), "{heard}");

    // Another origin answers without ever seeing the header.
    let output = run(
        &mut site.curl(),
        &["-fL", "-H", header, &near.url("elsewhere/dumb/x")],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), BODY);
    let heard = std::fs::read_to_string(&far_log).unwrap();
    assert!(heard.contains("GET /dumb/x?"), "{heard}");
    assert!(!heard.contains("auth=true"), "{heard}");

    // -O names the file from what was typed, not where the server sent it.
    let output = run(&mut site.curl(), &["-fLO", &near.url("moved/dumb/named")]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(site.path("named").exists());

    let output = run(&mut site.curl(), &["-L", &near.url("to-http/dumb/x")]);
    expect(&output, 1, "from https to plain http");

    // A redirect it cannot follow is a failure, never the redirect's own page.
    let output = run(
        &mut site.curl(),
        &["-fL", "-o", "out", &near.url("bad-location/x")],
    );
    expect(&output, 1, "cannot read where to");
    assert!(!site.path("out").exists());

    let far_enough = format!("{}dumb/x", "moved/".repeat(10));
    let output = run(&mut site.curl(), &["-fL", &near.url(&far_enough)]);
    assert!(output.status.success(), "{}", stderr(&output));
    let too_far = format!("{}dumb/x", "moved/".repeat(11));
    let output = run(&mut site.curl(), &["-fL", &near.url(&too_far)]);
    expect(&output, 1, "too many redirects");

    let output = run(
        &mut site.curl(),
        &["http://127.0.0.1:9/", "--proto", "=https"],
    );
    expect(&output, 1, "--proto");
}

#[test]
fn credentials_stay_out_of_a_url_and_out_of_messages() {
    let site = Site::new("credentials");
    let output = run(&mut site.curl(), &["https://me:s3cret@localhost:9/"]);
    expect(&output, 1, "may not hold credentials");
    assert!(!stderr(&output).contains("s3cret"), "{}", stderr(&output));
    let output = run(
        &mut site.curl(),
        &["--user=me:s3cret", "https://localhost:9/"],
    );
    expect(&output, 2, "--user is not supported");
    assert!(!stderr(&output).contains("s3cret"), "{}", stderr(&output));
}

/// A CONNECT proxy that remembers what it was asked.
fn proxy() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let heard = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&heard);
    std::thread::spawn(move || {
        for client in listener.incoming().flatten() {
            let log = Arc::clone(&log);
            std::thread::spawn(move || tunnel(client, &log));
        }
    });
    (url, heard)
}

fn tunnel(client: TcpStream, heard: &Mutex<Vec<String>>) {
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    heard.lock().unwrap().push(line.trim().to_string());
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).unwrap() == 0 || header.trim().is_empty() {
            break;
        }
    }
    let target = line.split_whitespace().nth(1).unwrap().to_string();
    let upstream = TcpStream::connect(target).unwrap();
    let mut answer = client.try_clone().unwrap();
    answer.write_all(b"HTTP/1.1 200 OK\r\n\r\n").unwrap();
    let copy = |mut from: Box<dyn Read + Send>, to: TcpStream| {
        std::thread::spawn(move || {
            let mut to = to;
            let _ = std::io::copy(&mut from, &mut to);
            let _ = to.shutdown(Shutdown::Write);
        })
    };
    let there = copy(Box::new(reader), upstream.try_clone().unwrap());
    let back = copy(Box::new(upstream), client);
    let _ = there.join();
    let _ = back.join();
}

#[test]
fn a_proxy_comes_from_the_environment_and_socks_is_refused() {
    let site = Site::new("proxy");
    let origin = site.serve(&[]);
    let url = origin.url("dumb/x");
    let output = run(
        site.curl().env("https_proxy", "http://127.0.0.1:9"),
        &[&url],
    );
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let output = run(
        site.curl()
            .env("https_proxy", "http://127.0.0.1:9")
            .env("no_proxy", "localhost"),
        &[&url],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let output = run(
        site.curl().env("all_proxy", "socks5://127.0.0.1:9"),
        &[&url],
    );
    expect(&output, 1, "SOCKS");
    let output = run(
        site.curl().env("https_proxy", "http://nowhere.test"),
        &[&url],
    );
    expect(&output, 1, "no port");

    let (via, heard) = proxy();
    let output = run(site.curl().env("https_proxy", &via), &[&url]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), BODY);
    let target = format!("CONNECT localhost:{} HTTP/1.1", origin.port);
    assert!(heard.lock().unwrap().contains(&target));

    // A proxy's password stays out of what is said about it.
    let secret = "http://me:s3cret@127.0.0.1:9";
    let output = run(site.curl().env("https_proxy", secret), &[&url]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(!stderr(&output).contains("s3cret"), "{}", stderr(&output));
}

#[test]
fn the_whole_operation_is_held_to_its_time() {
    let site = Site::new("time");
    let origin = site.serve(&[]);
    let started = Instant::now();
    let output = run(&mut site.curl(), &["-m", "1", &origin.url("trickle/x")]);
    expect(&output, 28, "td-curl (not curl): (28)");
    assert!(started.elapsed() < Duration::from_secs(20));

    // Across a redirect too: its budget is not started again.
    let started = Instant::now();
    let output = run(
        &mut site.curl(),
        &["-fLm", "1", &origin.url("moved/trickle/x")],
    );
    expect(&output, 28, "(28)");
    assert!(started.elapsed() < Duration::from_secs(20));
}
