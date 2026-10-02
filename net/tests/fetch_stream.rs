#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! td-fetchd's streamed replies end to end over the built td-net: each
//! test runs the service as a process of its own, `td-net fetchd run`
//! with `--allow-loopback` and the test-only bounds, since a streamed
//! request's exchange runs in a `td-fetchd origin` process the service
//! starts from its own binary. The origins are scripted per connection on
//! loopback.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const TD_NET: &str = env!("CARGO_BIN_EXE_td-net");
const PROTOCOL: &str = "td-fetch 1";
/// The service's frame bound.
const MAX_CHUNK: usize = 64 * 1024;
/// How long a scripted origin waits for its one connection.
const ORIGIN_PATIENCE: Duration = Duration::from_secs(20);

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("td-fetch-stream-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The service, killed with its directory when the test is done.
struct Service {
    child: Child,
    socket: PathBuf,
    dir: PathBuf,
}

impl Service {
    /// The lenient service with the given test bounds.
    fn start(tag: &str, bounds: &[&str]) -> Service {
        let mut flags = vec!["--allow-loopback"];
        flags.extend_from_slice(bounds);
        Service::with_flags(tag, &flags)
    }

    fn with_flags(tag: &str, flags: &[&str]) -> Service {
        let dir = scratch(tag);
        let socket = dir.join("td-fetch").join("socket");
        let child = Command::new(TD_NET)
            .args(["fetchd", "run", "--socket"])
            .arg(&socket)
            .args(flags)
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        // Held before the wait, so a service that never listens is killed.
        let service = Service { child, socket, dir };
        until("the service's socket", || {
            UnixStream::connect(&service.socket).is_ok()
        });
        service
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// One origin connection on loopback, answered by `script` once its
/// request is read; the request comes back through the handle. `None`
/// where the sandbox has no loopback to bind.
fn scripted_origin(
    script: impl FnOnce(&mut TcpStream) + Send + 'static,
) -> Option<(u16, std::thread::JoinHandle<Vec<u8>>)> {
    let listener = TcpListener::bind("127.0.0.1:0").ok()?;
    let port = listener.local_addr().ok()?.port();
    listener.set_nonblocking(true).ok()?;
    let handle = std::thread::spawn(move || {
        let deadline = Instant::now() + ORIGIN_PATIENCE;
        loop {
            match listener.accept() {
                Ok((mut conn, _)) => {
                    let _ = conn.set_nonblocking(false);
                    let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
                    let raw = read_http_request(&mut conn);
                    script(&mut conn);
                    return raw;
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => return Vec::new(),
            }
        }
    });
    Some((port, handle))
}

fn read_http_request(conn: &mut TcpStream) -> Vec<u8> {
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    while let Ok(n) = conn.read(&mut chunk) {
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..n]);
        if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&raw[..end]).to_ascii_lowercase();
            let length: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length: "))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            if raw.len() >= end + 4 + length {
                break;
            }
        }
    }
    raw
}

/// Waits, up to five seconds, for the origin's connection to close under
/// it, and says when it did.
fn closed_under(conn: &mut TcpStream) -> Option<Instant> {
    let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
    let mut scratch = [0u8; 64];
    loop {
        match conn.read(&mut scratch) {
            Ok(0) => return Some(Instant::now()),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {
                return Some(Instant::now())
            }
            Err(_) => return None,
        }
    }
}

fn response(status: &str, headers: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
        body.len()
    )
}

fn chunked_head(extra: &str) -> String {
    format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n{extra}\r\n")
}

fn http_chunk(bytes: &[u8]) -> Vec<u8> {
    let mut out = format!("{:x}\r\n", bytes.len()).into_bytes();
    out.extend_from_slice(bytes);
    out.extend_from_slice(b"\r\n");
    out
}

fn request_bytes(head: &str, body: &[u8]) -> Vec<u8> {
    let mut bytes = format!("{PROTOCOL}\n{head}\n").into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn open_stream(socket: &Path, head: &str, body: &[u8]) -> BufReader<UnixStream> {
    let mut stream = UnixStream::connect(socket).unwrap();
    // A service that never answers fails the test rather than hanging it.
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    stream.write_all(&request_bytes(head, body)).unwrap();
    BufReader::new(stream)
}

fn stream_get(port: u16, path: &str) -> String {
    format!("method GET\nurl http://127.0.0.1:{port}{path}\nstream\n")
}

/// A counted request's reply: its head lines and its body.
fn ask(socket: &Path, head: &str) -> (Vec<String>, Vec<u8>) {
    let mut reader = open_stream(socket, head, b"");
    let lines = read_head(&mut reader);
    let mut body = Vec::new();
    reader.read_to_end(&mut body).unwrap();
    (lines, body)
}

/// The reply's head lines, up to the blank line.
fn read_head(reader: &mut impl BufRead) -> Vec<String> {
    let mut lines = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let line = line.trim_end_matches('\n').to_string();
        if line.is_empty() {
            return lines;
        }
        lines.push(line);
    }
}

/// One frame's bytes, or the line that ended the stream. Every frame
/// keeps the bound.
fn read_frame(reader: &mut impl BufRead) -> Result<Vec<u8>, String> {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let line = line.trim_end_matches('\n');
    let Some(count) = line.strip_prefix("chunk ") else {
        return Err(line.to_string());
    };
    let count: usize = count.parse().unwrap();
    assert!((1..=MAX_CHUNK).contains(&count), "a frame of {count}");
    let mut bytes = vec![0u8; count];
    reader.read_exact(&mut bytes).unwrap();
    Ok(bytes)
}

/// At least `count` bytes of frames, however the service cut them.
fn read_bytes(reader: &mut impl BufRead, count: usize) -> Vec<u8> {
    let mut got = Vec::new();
    while got.len() < count {
        match read_frame(reader) {
            Ok(bytes) => got.extend_from_slice(&bytes),
            Err(last) => panic!("{last:?} after {} of {count} bytes", got.len()),
        }
    }
    got
}

/// The rest of the frames' bytes and the line that ended them, after
/// which the service has closed.
fn read_rest(reader: &mut impl BufRead) -> (Vec<u8>, String) {
    let mut body = Vec::new();
    loop {
        match read_frame(reader) {
            Ok(bytes) => body.extend_from_slice(&bytes),
            Err(last) => {
                let mut after = Vec::new();
                reader.read_to_end(&mut after).unwrap();
                assert!(after.is_empty(), "{after:?} after {last:?}");
                return (body, last);
            }
        }
    }
}

fn last_line(reader: &mut impl BufRead) -> Option<String> {
    read_head(reader).last().cloned()
}

#[test]
fn a_streamed_request_is_refused_as_a_counted_one_is() {
    // The strict service, as units run it.
    let service = Service::with_flags("refused", &[]);
    for (head, line) in [
        (
            "method GET\nurl http://127.0.0.1:1/\nstream\n",
            "error refused: loopback address".to_string(),
        ),
        (
            "method GET\nurl ftp://h/\nstream\n",
            "error refused: scheme \"ftp\"".to_string(),
        ),
        (
            "method GET\nurl http://0.0.0.1:1\\@93.184.216.34/\nstream\n",
            "error refused: unspecified address".to_string(),
        ),
        (
            "method GET\nurl http://h/\nredirects 6\nstream\n",
            "error refused: redirects over 5".to_string(),
        ),
        (
            "method POST\nurl http://h/\nredirects 1\nstream\nbody 0\n",
            "error malformed: a POST with redirects".to_string(),
        ),
        (
            "method GET\nurl http://h/\nlimit 0\nstream\n",
            "error malformed: limit \"0\"".to_string(),
        ),
    ] {
        let (lines, body) = ask(&service.socket, head);
        assert_eq!(lines, [PROTOCOL.to_string(), line], "{head:?}");
        assert!(body.is_empty(), "{head:?}");
    }
    // The test bounds are refused to a strict service.
    let refused = Command::new(TD_NET)
        .args([
            "fetchd",
            "run",
            "--socket",
            "/nonexistent/s",
            "--stream-total-ms",
            "5",
        ])
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("needs --allow-loopback"));
}

#[test]
fn a_stream_is_relayed_frame_by_frame_as_it_arrives() {
    let service = Service::start("frames", &[]);
    let rest = vec![b'x'; 3 * MAX_CHUNK / 2 + 7];
    let sent = rest.clone();
    let (go, wait) = mpsc::channel::<()>();
    let Some((port, origin)) = scripted_origin(move |conn| {
        let _ = conn.write_all(chunked_head("Content-Type: text/event-stream\r\n").as_bytes());
        let _ = conn.write_all(&http_chunk(b"data: one\n\n"));
        let _ = conn.flush();
        // The rest only once the client holds the first piece: what it
        // read was relayed as it came, not gathered.
        let _ = wait.recv_timeout(Duration::from_secs(10));
        let _ = conn.write_all(&http_chunk(&sent));
        let _ = conn.write_all(b"0\r\n\r\n");
        let _ = conn.flush();
    }) else {
        return;
    };
    let mut reader = open_stream(
        &service.socket,
        &format!(
            "method POST\nurl http://127.0.0.1:{port}/chat\nheader content-type: application/json\nstream\nbody 2\n"
        ),
        b"{}",
    );
    assert_eq!(
        read_head(&mut reader),
        [
            PROTOCOL,
            "status 200",
            "header content-type: text/event-stream",
            "stream"
        ]
    );
    assert_eq!(read_bytes(&mut reader, 11), b"data: one\n\n");
    go.send(()).unwrap();
    let (body, last) = read_rest(&mut reader);
    assert_eq!(body, rest);
    assert_eq!(last, "end");
    let request = String::from_utf8_lossy(&origin.join().unwrap()).to_ascii_lowercase();
    assert!(request.starts_with("post /chat http/1.1"), "{request}");
    assert!(request.contains("accept-encoding: identity"), "{request}");
    assert!(request.contains("user-agent: td-fetchd/1"), "{request}");
    assert!(
        request.contains("content-type: application/json"),
        "{request}"
    );
    assert!(request.ends_with("{}"), "{request}");
    // An answer over 400 streams as well, a counted body as a chunked one.
    let Some((port, origin)) = scripted_origin(|conn| {
        let _ = conn.write_all(response("429 Too Many Requests", "", "slow down").as_bytes());
    }) else {
        return;
    };
    let mut reader = open_stream(&service.socket, &stream_get(port, "/"), b"");
    let head = read_head(&mut reader);
    assert_eq!(head.get(1).map(String::as_str), Some("status 429"));
    assert_eq!(head.last().map(String::as_str), Some("stream"));
    assert_eq!(
        read_rest(&mut reader),
        (b"slow down".to_vec(), "end".to_string())
    );
    let _ = origin.join();
}

#[test]
fn a_streamed_body_keeps_the_limit_over_the_sum_of_its_frames() {
    let service = Service::start("limit", &[]);
    for (limit, last) in [
        (120, "end"),
        (100, "error refused: response over 100 bytes"),
    ] {
        let (go, wait) = mpsc::channel::<()>();
        let Some((port, origin)) = scripted_origin(move |conn| {
            let _ = conn.write_all(chunked_head("").as_bytes());
            let _ = conn.write_all(&http_chunk(&[b'a'; 60]));
            let _ = conn.flush();
            let _ = wait.recv_timeout(Duration::from_secs(10));
            let _ = conn.write_all(&http_chunk(&[b'b'; 60]));
            let _ = conn.write_all(b"0\r\n\r\n");
        }) else {
            return;
        };
        let mut reader = open_stream(
            &service.socket,
            &format!("method GET\nurl http://127.0.0.1:{port}/\nlimit {limit}\nstream\n"),
            b"",
        );
        assert_eq!(last_line(&mut reader).as_deref(), Some("stream"));
        assert_eq!(read_bytes(&mut reader, 60), [b'a'; 60]);
        go.send(()).unwrap();
        let (rest, ended) = read_rest(&mut reader);
        assert_eq!(ended, last, "limit {limit}");
        // What the client is given never passes the limit.
        if limit == 120 {
            assert_eq!(rest, [b'b'; 60]);
        } else {
            assert!(rest.is_empty(), "{rest:?}");
        }
        let _ = origin.join();
    }
}

#[test]
fn an_origin_that_breaks_off_mid_body_ends_the_stream_in_an_error_line() {
    let service = Service::start("broken", &[]);
    for (answer, received) in [
        // Ten bytes of a hundred promised, then the connection closes.
        (
            "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n0123456789"
                .to_string(),
            &b"0123456789"[..],
        ),
        // A chunk size that is not one.
        (
            format!("{}5\r\nhello\r\nzz\r\n", chunked_head("")),
            &b"hello"[..],
        ),
    ] {
        let Some((port, origin)) = scripted_origin(move |conn| {
            let _ = conn.write_all(answer.as_bytes());
        }) else {
            return;
        };
        let mut reader = open_stream(&service.socket, &stream_get(port, "/"), b"");
        assert_eq!(last_line(&mut reader).as_deref(), Some("stream"));
        let (body, last) = read_rest(&mut reader);
        assert_eq!(body, received);
        assert!(last.starts_with("error transport: read body: "), "{last}");
        let _ = origin.join();
    }
}

#[test]
fn a_silent_origin_ends_the_stream_at_the_idle_deadline() {
    let service = Service::start("idle", &["--stream-idle-ms", "300"]);
    // Silent mid-body: the frames so far, then the idle error line.
    let Some((port, origin)) = scripted_origin(|conn| {
        let _ = conn.write_all(chunked_head("").as_bytes());
        let _ = conn.write_all(&http_chunk(b"abc"));
        let _ = conn.flush();
        std::thread::sleep(Duration::from_secs(3));
    }) else {
        return;
    };
    let mut reader = open_stream(&service.socket, &stream_get(port, "/"), b"");
    assert_eq!(last_line(&mut reader).as_deref(), Some("stream"));
    assert_eq!(read_bytes(&mut reader, 3), b"abc");
    let silent = Instant::now();
    let (rest, last) = read_rest(&mut reader);
    assert!(rest.is_empty(), "{rest:?}");
    assert_eq!(last, "error transport: the origin sent nothing for 300ms");
    assert!(
        silent.elapsed() < Duration::from_secs(2),
        "{:?}",
        silent.elapsed()
    );
    let _ = origin.join();
    // Silent before its head: the `error` head a counted request gets.
    let Some((port, origin)) = scripted_origin(|_| std::thread::sleep(Duration::from_secs(3)))
    else {
        return;
    };
    let asked = Instant::now();
    let mut reader = open_stream(&service.socket, &stream_get(port, "/"), b"");
    let head = read_head(&mut reader);
    assert_eq!(head.len(), 2, "{head:?}");
    assert!(head[1].starts_with("error transport: "), "{head:?}");
    assert!(
        asked.elapsed() < Duration::from_secs(2),
        "{:?}",
        asked.elapsed()
    );
    let _ = origin.join();
}

/// A redirect answered on a kept-alive connection would be followed on
/// that connection, whose timeouts ureq clears when it pools it; the
/// streamed exchange pools nothing, so the idle deadline holds on the next
/// hop.
#[test]
fn a_redirect_on_a_kept_connection_keeps_the_idle_deadline() {
    let service = Service::start(
        "redirect",
        &["--stream-idle-ms", "300", "--stream-total-ms", "5000"],
    );
    // A pooling client asks for the second hop here and waits on this
    // silence; one that pools nothing asks on a new connection, which the
    // listener's backlog holds unanswered.
    let Some((port, origin)) = scripted_origin(|conn| {
        let _ =
            conn.write_all(b"HTTP/1.1 302 Found\r\nLocation: /next\r\nContent-Length: 0\r\n\r\n");
        let _ = conn.flush();
        let _ = read_http_request(conn);
        std::thread::sleep(Duration::from_secs(3));
    }) else {
        return;
    };
    let started = Instant::now();
    let mut reader = open_stream(&service.socket, &stream_get(port, "/first"), b"");
    let head = read_head(&mut reader);
    assert_eq!(head.len(), 2, "{head:?}");
    assert!(head[1].starts_with("error transport: "), "{head:?}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    let _ = origin.join();
}

#[test]
fn an_origin_that_never_falls_silent_ends_at_the_total() {
    let service = Service::start(
        "total",
        &["--stream-idle-ms", "2000", "--stream-total-ms", "1000"],
    );
    // A piece every 50 ms, never idle, for three seconds; the origin sees
    // its connection go when the total kills the exchange.
    let (closed, heard) = mpsc::channel::<Instant>();
    let Some((port, origin)) = scripted_origin(move |conn| {
        let _ = conn.write_all(chunked_head("").as_bytes());
        for _ in 0..60 {
            if conn.write_all(&http_chunk(b"a")).is_err() {
                let _ = closed.send(Instant::now());
                return;
            }
            let _ = conn.flush();
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = conn.write_all(b"0\r\n\r\n");
    }) else {
        return;
    };
    let asked = Instant::now();
    let mut reader = open_stream(&service.socket, &stream_get(port, "/"), b"");
    assert_eq!(last_line(&mut reader).as_deref(), Some("stream"));
    let (body, last) = read_rest(&mut reader);
    assert_eq!(last, "error transport: the stream ran past its 1s total");
    assert!(!body.is_empty() && body.len() < 60, "{}", body.len());
    assert!(
        asked.elapsed() < Duration::from_secs(3),
        "{:?}",
        asked.elapsed()
    );
    let when = heard.recv_timeout(Duration::from_secs(4)).unwrap();
    assert!(when.duration_since(asked) < Duration::from_secs(3));
    let _ = origin.join();
    // A head a byte at a time is answered at the total too, and the
    // origin's connection goes with the exchange rather than outliving it.
    let (closed, heard) = mpsc::channel::<Instant>();
    let Some((port, origin)) = scripted_origin(move |conn| {
        let head = format!("HTTP/1.1 200 OK\r\nX-Pad: {}\r\n\r\n", "p".repeat(40));
        for byte in head.as_bytes() {
            if conn.write_all(&[*byte]).is_err() {
                let _ = closed.send(Instant::now());
                return;
            }
            let _ = conn.flush();
            std::thread::sleep(Duration::from_millis(50));
        }
    }) else {
        return;
    };
    let asked = Instant::now();
    let mut reader = open_stream(&service.socket, &stream_get(port, "/"), b"");
    assert_eq!(
        read_head(&mut reader),
        [
            PROTOCOL,
            "error transport: the stream ran past its 1s total"
        ]
    );
    assert!(
        asked.elapsed() < Duration::from_secs(3),
        "{:?}",
        asked.elapsed()
    );
    let when = heard.recv_timeout(Duration::from_secs(4)).unwrap();
    assert!(when.duration_since(asked) < Duration::from_secs(3));
    let _ = origin.join();
}

/// A client that stops reading but keeps its connection open does not keep
/// the origin's exchange past the total: writing to it is held to the
/// total as well as to the client's budget, here the longer.
#[test]
fn a_client_that_stops_reading_does_not_hold_the_origin_past_the_total() {
    let service = Service::start(
        "stalled",
        &[
            "--client-ms",
            "8000",
            "--stream-idle-ms",
            "5000",
            "--stream-total-ms",
            "1000",
        ],
    );
    // As fast as it can, until its connection goes.
    let (closed, heard) = mpsc::channel::<Instant>();
    let Some((port, origin)) = scripted_origin(move |conn| {
        let _ = conn.write_all(chunked_head("").as_bytes());
        let piece = http_chunk(&[b'z'; 16 * 1024]);
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(15) {
            if conn.write_all(&piece).is_err() {
                let _ = closed.send(Instant::now());
                return;
            }
        }
    }) else {
        return;
    };
    let asked = Instant::now();
    let mut reader = open_stream(&service.socket, &stream_get(port, "/"), b"");
    assert_eq!(last_line(&mut reader).as_deref(), Some("stream"));
    // Read nothing more, and hold the connection open.
    let when = heard
        .recv_timeout(Duration::from_secs(10))
        .expect("the origin's connection outlived the total");
    assert!(
        when.duration_since(asked) < Duration::from_secs(4),
        "{:?}",
        when.duration_since(asked)
    );
    drop(reader);
    let _ = origin.join();
}

/// A client that hangs up while the relay waits to write to it, having
/// stopped reading, is seen to go as promptly as one that hangs up during
/// the origin's silence; closing only its writing half is a hang-up.
#[test]
fn a_client_that_hangs_up_while_a_write_waits_ends_the_stream() {
    let service = Service::start(
        "stalled-hangup",
        &[
            "--client-ms",
            "8000",
            "--stream-idle-ms",
            "10000",
            "--stream-total-ms",
            "20000",
        ],
    );
    let (closed, heard) = mpsc::channel::<Instant>();
    let Some((port, origin)) = scripted_origin(move |conn| {
        let _ = conn.write_all(chunked_head("").as_bytes());
        let piece = http_chunk(&[b'z'; 16 * 1024]);
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(15) {
            if conn.write_all(&piece).is_err() {
                let _ = closed.send(Instant::now());
                return;
            }
        }
    }) else {
        return;
    };
    let mut reader = open_stream(&service.socket, &stream_get(port, "/"), b"");
    assert_eq!(last_line(&mut reader).as_deref(), Some("stream"));
    // Every buffer between the relay and here fills, and its write waits.
    std::thread::sleep(Duration::from_secs(1));
    reader
        .get_ref()
        .shutdown(std::net::Shutdown::Write)
        .unwrap();
    let hung_up = Instant::now();
    let when = heard
        .recv_timeout(Duration::from_secs(12))
        .expect("the origin's connection outlived the client's");
    assert!(
        when.duration_since(hung_up) < Duration::from_secs(3),
        "{:?}",
        when.duration_since(hung_up)
    );
    drop(reader);
    let _ = origin.join();
}

/// ureq grows a chunk-size line without a bound; an origin that sends one
/// at full speed is stopped by the origin process's memory bound, not by
/// any deadline, and its connection goes with the process.
#[test]
fn an_endless_chunk_size_line_meets_the_memory_bound() {
    let service = Service::start(
        "memory",
        &[
            "--stream-memory-mib",
            "32",
            "--stream-idle-ms",
            "5000",
            "--stream-total-ms",
            "20000",
        ],
    );
    let (closed, heard) = mpsc::channel::<Instant>();
    let Some((port, origin)) = scripted_origin(move |conn| {
        let _ = conn.write_all(chunked_head("").as_bytes());
        let digits = vec![b'1'; 64 * 1024];
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(15) {
            if conn.write_all(&digits).is_err() {
                let _ = closed.send(Instant::now());
                return;
            }
        }
    }) else {
        return;
    };
    let asked = Instant::now();
    let mut reader = open_stream(&service.socket, &stream_get(port, "/"), b"");
    assert_eq!(last_line(&mut reader).as_deref(), Some("stream"));
    let (body, last) = read_rest(&mut reader);
    assert!(body.is_empty());
    assert_eq!(
        last,
        "error refused: the exchange with the origin passed its 32 MiB memory bound"
    );
    assert!(
        asked.elapsed() < Duration::from_secs(10),
        "{:?}",
        asked.elapsed()
    );
    heard.recv_timeout(Duration::from_secs(5)).unwrap();
    let _ = origin.join();
}

/// The client's budget is for writing to it, not for the origin's wait: a
/// head slower than the budget, within the idle deadline, is relayed.
#[test]
fn a_head_slower_than_the_client_budget_is_relayed() {
    let service = Service::start("slow-head", &["--client-ms", "1000"]);
    let Some((port, origin)) = scripted_origin(|conn| {
        std::thread::sleep(Duration::from_millis(1500));
        let _ = conn.write_all(response("200 OK", "", "late").as_bytes());
    }) else {
        return;
    };
    let mut reader = open_stream(&service.socket, &stream_get(port, "/"), b"");
    assert_eq!(
        read_head(&mut reader),
        [PROTOCOL, "status 200", "header content-length: 4", "stream"]
    );
    assert_eq!(
        read_rest(&mut reader),
        (b"late".to_vec(), "end".to_string())
    );
    let _ = origin.join();
}

/// A client that hangs up ends its exchange, the origin's connection with
/// it, whether the origin is sending or silent: a silent one is seen to go
/// between the relay's waits rather than at the next frame.
#[test]
fn a_client_that_hangs_up_closes_the_origin_connection() {
    let service = Service::start("hangup", &["--stream-idle-ms", "10000"]);
    for silent in [false, true] {
        let (closed, heard) = mpsc::channel::<Instant>();
        let Some((port, origin)) = scripted_origin(move |conn| {
            let _ = conn.write_all(chunked_head("").as_bytes());
            let _ = conn.write_all(&http_chunk(b"tick"));
            let _ = conn.flush();
            if silent {
                if let Some(when) = closed_under(conn) {
                    let _ = closed.send(when);
                }
                return;
            }
            for _ in 0..200 {
                if conn.write_all(&http_chunk(b"tick")).is_err() {
                    let _ = closed.send(Instant::now());
                    return;
                }
                let _ = conn.flush();
                std::thread::sleep(Duration::from_millis(25));
            }
        }) else {
            return;
        };
        let mut reader = open_stream(&service.socket, &stream_get(port, "/"), b"");
        assert_eq!(last_line(&mut reader).as_deref(), Some("stream"));
        read_bytes(&mut reader, 4);
        drop(reader);
        let hung_up = Instant::now();
        let when = heard
            .recv_timeout(Duration::from_secs(4))
            .expect("the origin's connection outlived the client's");
        assert!(
            when.duration_since(hung_up) < Duration::from_secs(2),
            "silent {silent}"
        );
        let _ = origin.join();
    }
    // The service goes on.
    let (lines, _) = ask(&service.socket, "method GET\nurl ftp://127.0.0.1/\n");
    assert_eq!(lines[1], "error refused: scheme \"ftp\"");
}

/// Streams do not hold the service's workers: with every stream place
/// taken, twice the workers, each stream's head, a counted request and a
/// ninth stream's refusal are answered at once.
#[test]
fn open_streams_leave_the_workers_to_counted_requests() {
    let service = Service::start(
        "workers",
        &["--stream-idle-ms", "20000", "--stream-total-ms", "30000"],
    );
    let mut open = Vec::new();
    for _ in 0..8 {
        // Each origin holds its stream open until the test lets it go.
        let (release, held) = mpsc::channel::<()>();
        let Some((port, origin)) = scripted_origin(move |conn| {
            let _ = conn.write_all(chunked_head("").as_bytes());
            let _ = conn.flush();
            let _ = held.recv_timeout(Duration::from_secs(20));
        }) else {
            return;
        };
        let asked = Instant::now();
        let mut reader = open_stream(&service.socket, &stream_get(port, "/"), b"");
        assert_eq!(last_line(&mut reader).as_deref(), Some("stream"));
        assert!(
            asked.elapsed() < Duration::from_secs(2),
            "stream {}: {:?}",
            open.len(),
            asked.elapsed()
        );
        open.push((reader, release, origin));
    }
    let asked = Instant::now();
    let (lines, _) = ask(&service.socket, "method GET\nurl ftp://127.0.0.1/\n");
    assert_eq!(lines[1], "error refused: scheme \"ftp\"");
    let (lines, body) = ask(&service.socket, &stream_get(1, "/"));
    assert_eq!(
        lines,
        [
            PROTOCOL,
            "error refused: all 8 places for streamed exchanges are taken"
        ]
    );
    assert!(body.is_empty());
    assert!(
        asked.elapsed() < Duration::from_secs(1),
        "{:?}",
        asked.elapsed()
    );
    for (reader, release, origin) in open {
        drop(reader);
        drop(release);
        let _ = origin.join();
    }
}

/// The origin process dies with the service: killed outright, the service
/// signals nothing, and the process sees its parent go and ends, closing
/// the origin's connection.
#[test]
fn an_origin_process_ends_with_the_service() {
    let mut service = Service::start("parent", &["--stream-idle-ms", "10000"]);
    let (closed, heard) = mpsc::channel::<Instant>();
    let Some((port, origin)) = scripted_origin(move |conn| {
        let _ = conn.write_all(chunked_head("").as_bytes());
        let _ = conn.flush();
        if let Some(when) = closed_under(conn) {
            let _ = closed.send(when);
        }
    }) else {
        return;
    };
    let mut reader = open_stream(&service.socket, &stream_get(port, "/"), b"");
    assert_eq!(last_line(&mut reader).as_deref(), Some("stream"));
    service.child.kill().unwrap();
    service.child.wait().unwrap();
    let killed = Instant::now();
    let when = heard
        .recv_timeout(Duration::from_secs(4))
        .expect("the origin process outlived the service");
    assert!(when.duration_since(killed) < Duration::from_secs(2));
    let _ = origin.join();
}
