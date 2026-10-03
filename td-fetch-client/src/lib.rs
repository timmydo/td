//! The td fetch service, as a client: HTTP through the unix socket
//! `$XDG_RUNTIME_DIR/td-fetch/socket`, whose directory td-jail binds into a
//! jail that carries `sockets=fetch` (td's APPLICATIONS.md §W.8); the
//! directory rather than the socket inode, so a restarted service's fresh
//! socket is at the same path. The service holds the TLS trust, the
//! resolver, the timeouts and the body caps; this side holds a socket and
//! the framing, in `std` alone: one crate td's applications depend on by
//! path (AGENTS.md principle 2).
//!
//! One request per connection: a text head, a blank line, the body; back,
//! a text head, a blank line, the body, or an `error` line the service
//! wrote so a person can be told what happened. The service may answer a
//! bad head before the body is through, so the request is written whole
//! before anything is read, and a failed write is not the outcome: the
//! reply is.
//!
//! A streamed request (`get_stream`, `post_stream`) asks for the body as
//! the origin sends it: the reply head ends in `stream` rather than
//! `body N`, and the body comes as `chunk N` frames of at most 64 KiB each,
//! closed by an `end` line or an `error` line.

#![forbid(unsafe_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

const PROTOCOL: &str = "td-fetch 1";
const SOCKET_DIRECTORY: &str = "td-fetch";
const SOCKET_FILE: &str = "socket";
/// A head line in either direction, the service's bound.
const MAX_LINE: usize = 8 * 1024;
/// The service's own ceiling, asked for when the caller names none.
const DEFAULT_LIMIT: u64 = 64 * 1024 * 1024;
/// The most a request body may carry: the service refuses one past it.
#[allow(dead_code)]
pub const MAX_REQUEST_BODY: u64 = 32 * 1024 * 1024;
/// The service's bound on one streamed frame.
#[allow(dead_code)]
const MAX_CHUNK: usize = 64 * 1024;
/// The service answers a counted request within its budgets (a minute for
/// the head, five for the origin); this, the longest any one read or write
/// here waits, bounds a service that has gone away mid-reply, and a stream
/// whose origin keeps it alive without a frame, which the service would
/// end only at its total.
const REPLY_TIMEOUT: Duration = Duration::from_secs(420);
/// The wait for a streamed reply's head: the service's thirty-minute total
/// for a stream, and a minute for the service itself.
const STREAM_HEAD_TIMEOUT: Duration = Duration::from_secs(31 * 60);
const NO_REPLY: &str = "the service closed without a reply";

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    /// Names in lower case, in the order the origin sent them.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    /// The first value of `name` (lower case), if any.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// A streamed reply: its head, then its body a frame at a time as the
/// origin sends it. Dropping it closes the connection, which is how a
/// caller stops a stream early; the service then closes the origin's.
/// Neither application streams yet; the stream's items, like `post`, stay
/// in the one text all the same.
#[allow(dead_code)]
#[derive(Debug)]
pub struct Stream {
    pub status: u16,
    /// Names in lower case, in the order the origin sent them.
    pub headers: Vec<(String, String)>,
    reader: BufReader<UnixStream>,
    frames: Frames,
}

#[allow(dead_code)]
impl Stream {
    /// The first value of `name` (lower case), if any.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The next piece of the body, at most 64 KiB, or `None` once the
    /// service has said the body is whole. An error ends the stream as
    /// well: every call after an error or a `None` is `None`.
    pub fn next_chunk(&mut self) -> Result<Option<&[u8]>, Error> {
        self.frames.next(&mut self.reader)
    }
}

/// The body side of a stream: what was asked for, what has come, and one
/// frame's buffer, reused.
#[allow(dead_code)]
#[derive(Debug)]
struct Frames {
    limit: u64,
    received: u64,
    buffer: Vec<u8>,
    done: bool,
}

#[allow(dead_code)]
impl Frames {
    fn new(limit: u64) -> Frames {
        Frames {
            limit,
            received: 0,
            buffer: Vec::new(),
            done: false,
        }
    }

    fn next(&mut self, reader: &mut impl BufRead) -> Result<Option<&[u8]>, Error> {
        if self.done {
            return Ok(None);
        }
        // Whatever this frame turns out to be, nothing after a failed one
        // can be read as framing; a good one reopens the stream below.
        self.done = true;
        let line = read_line(reader).map_err(|e| match e {
            Error::Io(m) if m == NO_REPLY => Error::Io("the service closed mid-stream".into()),
            other => other,
        })?;
        if line == "end" {
            return Ok(None);
        }
        if let Some(value) = line.strip_prefix("error ") {
            return Err(fault(value));
        }
        let count = line
            .strip_prefix("chunk ")
            .ok_or_else(|| Error::Io(format!("stream line {line:?}")))?;
        // Digits as the service writes them, no sign and no leading zero.
        let count = Some(count)
            .filter(|text| text.bytes().all(|b| b.is_ascii_digit()) && !text.starts_with('0'))
            .and_then(|text| text.parse::<usize>().ok())
            .filter(|count| (1..=MAX_CHUNK).contains(count))
            .ok_or_else(|| Error::Io(format!("chunk {count:?}")))?;
        // The service applies the limit; this is the client not taking its
        // word for it.
        let received = self.received.saturating_add(count as u64);
        if received > self.limit {
            return Err(Error::Io(format!(
                "the service sent {received} bytes past the {} asked for",
                self.limit
            )));
        }
        self.buffer.resize(count, 0);
        reader
            .read_exact(&mut self.buffer)
            .map_err(|e| Error::Io(format!("read chunk: {e}")))?;
        self.received = received;
        self.done = false;
        Ok(Some(&self.buffer))
    }
}

#[derive(Debug)]
pub enum Error {
    /// The service refused the request, for the reason it gave.
    Refused(String),
    /// The service found the request malformed: this client's bug.
    Malformed(String),
    /// The network's answer, as the service saw it.
    Transport(String),
    /// The socket, or the reply, could not be read as the protocol.
    Io(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Refused(m) => write!(f, "refused by the fetch service: {m}"),
            Error::Malformed(m) => write!(f, "the fetch service found the request malformed: {m}"),
            Error::Transport(m) => write!(f, "{m}"),
            Error::Io(m) => write!(f, "fetch service: {m}"),
        }
    }
}

/// `$XDG_RUNTIME_DIR/td-fetch/socket`, when it is a socket: the grant's
/// whole contract, with no variable of its own.
pub fn socket_path() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let path = PathBuf::from(runtime)
        .join(SOCKET_DIRECTORY)
        .join(SOCKET_FILE);
    let meta = std::fs::metadata(&path).ok()?;
    meta.file_type().is_socket().then_some(path)
}

pub fn available() -> bool {
    socket_path().is_some()
}

/// GET `url` with `headers` (names in lower case), taking at most `limit`
/// body bytes (the service's ceiling when `None`), the service following at
/// most `redirects` (its own five when `None`). The service drops
/// `authorization` on a redirect it follows; a client that carries one asks
/// for `Some(0)` and follows the `location` it is handed itself.
pub fn get(
    url: &str,
    headers: &[(&str, &str)],
    limit: Option<u64>,
    redirects: Option<u32>,
) -> Result<Response, Error> {
    request("GET", url, headers, &[], limit, redirects)
}

/// POST `body` to `url` with `headers` (names in lower case). A reader has
/// no POST; the module is one text in both applications.
#[allow(dead_code)]
pub fn post(
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    limit: Option<u64>,
) -> Result<Response, Error> {
    request("POST", url, headers, body, limit, None)
}

/// `get`, with the body handed over as the origin sends it, `limit`
/// bounding the frames' sum.
#[allow(dead_code)]
pub fn get_stream(
    url: &str,
    headers: &[(&str, &str)],
    limit: Option<u64>,
    redirects: Option<u32>,
) -> Result<Stream, Error> {
    stream("GET", url, headers, &[], limit, redirects)
}

/// `post`, with the reply's body handed over as the origin sends it: a
/// model's tokens as they are generated, say.
#[allow(dead_code)]
pub fn post_stream(
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    limit: Option<u64>,
) -> Result<Stream, Error> {
    stream("POST", url, headers, body, limit, None)
}

fn request(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    limit: Option<u64>,
    redirects: Option<u32>,
) -> Result<Response, Error> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT);
    let (mut reader, written) = send(method, url, headers, body, limit, redirects, false)?;
    settle(read_reply(&mut reader, limit), written)
}

fn stream(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    limit: Option<u64>,
    redirects: Option<u32>,
) -> Result<Stream, Error> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT);
    let (mut reader, written) = send(method, url, headers, body, limit, redirects, true)?;
    // A streamed head may take the service's whole total to come, the
    // origin's every step within its idle deadline; the frames then come
    // within that deadline of each other.
    let timed = |reader: &BufReader<UnixStream>, wait: Duration| {
        reader
            .get_ref()
            .set_read_timeout(Some(wait))
            .map_err(|e| Error::Io(e.to_string()))
    };
    timed(&reader, STREAM_HEAD_TIMEOUT)?;
    let head = read_stream_head(&mut reader);
    let (status, headers) = settle(head, written)?;
    timed(&reader, REPLY_TIMEOUT)?;
    Ok(Stream {
        status,
        headers,
        reader,
        frames: Frames::new(limit),
    })
}

/// Connect and write the request whole: the reader for the reply, and how
/// the write went, which matters only if the reply says nothing.
fn send(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    limit: u64,
    redirects: Option<u32>,
    stream: bool,
) -> Result<(BufReader<UnixStream>, std::io::Result<()>), Error> {
    check_head(url, headers)?;
    let path = socket_path().ok_or_else(|| Error::Io("no td-fetch socket".into()))?;
    let mut socket = UnixStream::connect(&path).map_err(|e| Error::Io(format!("connect: {e}")))?;
    // Both directions: a service that stopped reading a large body would
    // otherwise hold the writer past the reply's own bound.
    socket
        .set_read_timeout(Some(REPLY_TIMEOUT))
        .and_then(|()| socket.set_write_timeout(Some(REPLY_TIMEOUT)))
        .map_err(|e| Error::Io(e.to_string()))?;
    let mut head = format!("{PROTOCOL}\nmethod {method}\nurl {url}\n");
    for (name, value) in headers {
        head.push_str("header ");
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push('\n');
    }
    head.push_str(&format!("limit {limit}\n"));
    if let Some(redirects) = redirects {
        head.push_str(&format!("redirects {redirects}\n"));
    }
    if stream {
        head.push_str("stream\n");
    }
    head.push_str(&format!("body {}\n\n", body.len()));
    // Written whole before anything is read; the service may already have
    // answered and closed, and then the reply is what matters, not EPIPE.
    let written = socket
        .write_all(head.as_bytes())
        .and_then(|()| socket.write_all(body))
        .and_then(|()| socket.flush());
    Ok((BufReader::new(socket), written))
}

fn settle<T>(reply: Result<T, Error>, written: std::io::Result<()>) -> Result<T, Error> {
    match (reply, written) {
        (Ok(reply), _) => Ok(reply),
        (Err(error), Ok(())) => Err(error),
        // The write broke because the service closed its end. If it said
        // anything first, that is the answer; only silence leaves the
        // broken pipe as the best account of what happened.
        (Err(Error::Io(silence)), Err(write_error)) if silence == NO_REPLY => {
            Err(Error::Io(format!("write: {write_error}")))
        }
        (Err(verdict), Err(_)) => Err(verdict),
    }
}

/// The head is newline-framed `key value` lines. A URL or header name
/// carrying a line break would end its line early and write the next one
/// itself; a name carrying the separators would be read as some other
/// name with some other value.
fn check_head(url: &str, headers: &[(&str, &str)]) -> Result<(), Error> {
    if url.bytes().any(|b| b.is_ascii_control()) {
        return Err(Error::Malformed("the url carries a control byte".into()));
    }
    for (name, value) in headers {
        if name
            .bytes()
            .any(|b| b.is_ascii_control() || b == b':' || b == b' ')
        {
            return Err(Error::Malformed(format!(
                "header name {name:?} carries a separator or a control byte"
            )));
        }
        // A tab is field content (RFC 9110 §5.5); the rest of the control
        // range is not.
        if value.bytes().any(|b| b.is_ascii_control() && b != b'\t') {
            return Err(Error::Malformed(format!(
                "header {name:?} carries a control byte"
            )));
        }
    }
    Ok(())
}

fn read_line(reader: &mut impl BufRead) -> Result<String, Error> {
    let mut raw = Vec::with_capacity(128);
    let read = reader
        .by_ref()
        .take(MAX_LINE as u64)
        .read_until(b'\n', &mut raw)
        .map_err(|e| Error::Io(format!("read: {e}")))?;
    if read == 0 {
        return Err(Error::Io(NO_REPLY.into()));
    }
    if raw.last() != Some(&b'\n') {
        return Err(Error::Io("a reply line past the bound".into()));
    }
    raw.pop();
    String::from_utf8(raw).map_err(|_| Error::Io("a reply line that is not UTF-8".into()))
}

/// How a reply head said its body comes.
enum Framing {
    /// Counted: this many bytes after the head.
    Body(u64),
    /// As `chunk` frames to an `end` or `error` line.
    Stream,
}

struct Head {
    status: u16,
    headers: Vec<(String, String)>,
    framing: Framing,
}

/// An `error` line's `kind: reason`, as the error it names.
fn fault(value: &str) -> Error {
    let (kind, reason) = value.split_once(": ").unwrap_or((value, ""));
    match kind {
        "refused" => Error::Refused(reason.to_string()),
        "malformed" => Error::Malformed(reason.to_string()),
        "transport" => Error::Transport(reason.to_string()),
        _ => Error::Io(format!("error {value:?}")),
    }
}

fn read_head(reader: &mut impl BufRead) -> Result<Head, Error> {
    let first = read_line(reader)?;
    if first != PROTOCOL {
        return Err(Error::Io(format!(
            "the reply began {first:?}, not {PROTOCOL:?}"
        )));
    }
    let mut status = None;
    let mut headers = Vec::new();
    let mut framing = None;
    loop {
        let line = read_line(reader)?;
        if line.is_empty() {
            break;
        }
        let (key, value) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        match key {
            "status" => {
                status = Some(
                    value
                        .parse::<u16>()
                        .map_err(|_| Error::Io(format!("status {value:?}")))?,
                );
            }
            "header" => {
                let (name, value) = value
                    .split_once(':')
                    .ok_or_else(|| Error::Io(format!("header line {value:?}")))?;
                headers.push((
                    name.to_string(),
                    value.strip_prefix(' ').unwrap_or(value).to_string(),
                ));
            }
            "body" | "stream" if framing.is_some() => {
                return Err(Error::Io("a reply framed twice".into()));
            }
            "body" => {
                framing = Some(Framing::Body(
                    value
                        .parse::<u64>()
                        .map_err(|_| Error::Io(format!("body {value:?}")))?,
                ));
            }
            "stream" if line == "stream" => framing = Some(Framing::Stream),
            "error" => return Err(fault(value)),
            other => return Err(Error::Io(format!("reply key {other:?}"))),
        }
    }
    let status = status.ok_or_else(|| Error::Io("a reply with no status".into()))?;
    Ok(Head {
        status,
        headers,
        framing: framing.unwrap_or(Framing::Body(0)),
    })
}

fn read_reply(reader: &mut impl BufRead, limit: u64) -> Result<Response, Error> {
    let Head {
        status,
        headers,
        framing,
    } = read_head(reader)?;
    let Framing::Body(body_len) = framing else {
        return Err(Error::Io(
            "a streamed reply to a request for one body".into(),
        ));
    };
    // The service applies the limit; this is the client not taking its
    // word for it.
    if body_len > limit {
        return Err(Error::Io(format!(
            "the service announced {body_len} bytes past the {limit} asked for"
        )));
    }
    let mut body = Vec::new();
    reader
        .take(body_len)
        .read_to_end(&mut body)
        .map_err(|e| Error::Io(format!("read body: {e}")))?;
    if body.len() as u64 != body_len {
        return Err(Error::Io(format!(
            "the body was {} bytes of the {body_len} announced",
            body.len()
        )));
    }
    Ok(Response {
        status,
        headers,
        body,
    })
}

/// A streamed reply's head; its frames are the `Stream`'s to read.
fn read_stream_head(reader: &mut impl BufRead) -> Result<(u16, Vec<(String, String)>), Error> {
    let head = read_head(reader)?;
    match head.framing {
        Framing::Stream => Ok((head.status, head.headers)),
        Framing::Body(_) => Err(Error::Io(
            "one body in reply to a request for a stream".into(),
        )),
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    /// Held by every test that points `XDG_RUNTIME_DIR` at a directory of
    /// its own: the environment is one per process and the tests run in
    /// parallel.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());
        ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn reply(bytes: &[u8]) -> Result<Response, Error> {
        read_reply(&mut BufReader::new(bytes), DEFAULT_LIMIT)
    }

    /// A body the service announces past what was asked for is refused
    /// before it is read.
    #[test]
    fn a_body_past_the_limit_is_not_read() {
        let wire = format!("{PROTOCOL}\nstatus 200\nbody 11\n\nhello world");
        let err = read_reply(&mut BufReader::new(wire.as_bytes()), 10).unwrap_err();
        assert!(
            matches!(err, Error::Io(ref m) if m.contains("11 bytes past the 10")),
            "{err}"
        );
        let ok = read_reply(&mut BufReader::new(wire.as_bytes()), 11).unwrap();
        assert_eq!(ok.body, b"hello world");
    }

    /// What would frame a line of its own never reaches the head: a URL
    /// with a line break, a header name with the separator, a value with
    /// a control byte.
    #[test]
    fn the_head_refuses_what_would_write_its_own_line() {
        assert!(check_head("https://h/p?q=1", &[("accept", "text/xml")]).is_ok());
        assert!(check_head("https://h/p", &[("x-note", "a\tb")]).is_ok());
        for (url, headers) in [
            ("https://h/x\nheader a: b", &[][..]),
            ("https://h/x", &[("a:b", "c")][..]),
            ("https://h/x", &[("a b", "c")][..]),
            ("https://h/x", &[("a", "c\r")][..]),
        ] {
            let err = check_head(url, headers).unwrap_err();
            assert!(
                matches!(err, Error::Malformed(_)),
                "{url:?} {headers:?}: {err}"
            );
        }
    }

    #[test]
    fn a_reply_is_read_as_the_service_writes_it() {
        let response = reply(
            b"td-fetch 1\nstatus 200\nheader content-type: text/xml\nheader x-two: a\nbody 6\n\n<rss/>",
        )
        .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.header("content-type"), Some("text/xml"));
        assert_eq!(response.header("x-two"), Some("a"));
        assert_eq!(response.header("missing"), None);
        assert_eq!(response.body, b"<rss/>");
        let response = reply(b"td-fetch 1\nstatus 204\nbody 0\n\n").unwrap();
        assert_eq!((response.status, response.body.len()), (204, 0));
        assert!(matches!(
            reply(b"td-fetch 1\nerror refused: loopback address\n\n"),
            Err(Error::Refused(m)) if m == "loopback address"
        ));
        assert!(matches!(
            reply(b"td-fetch 1\nerror malformed: no url\n\n"),
            Err(Error::Malformed(m)) if m == "no url"
        ));
        assert!(matches!(
            reply(b"td-fetch 1\nerror transport: Dns Failed\n\n"),
            Err(Error::Transport(m)) if m == "Dns Failed"
        ));
        assert!(matches!(reply(b""), Err(Error::Io(_))));
        assert!(matches!(reply(b"td-fetch 2\n\n"), Err(Error::Io(_))));
        assert!(matches!(
            reply(b"td-fetch 1\nbody 0\n\n"),
            Err(Error::Io(_))
        ));
        assert!(matches!(
            reply(b"td-fetch 1\nstatus 200\nbody 5\n\nab"),
            Err(Error::Io(_))
        ));
    }

    #[test]
    fn the_socket_is_found_under_the_runtime_directory_only_as_a_socket() {
        let _env = env_lock();
        let dir = std::env::temp_dir().join(format!("td-fetch-client-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(SOCKET_DIRECTORY)).unwrap();
        let socket = dir.join(SOCKET_DIRECTORY).join(SOCKET_FILE);
        // This test owns the variable for its duration, under the guard the
        // crate's other environment-setting tests hold.
        std::env::set_var("XDG_RUNTIME_DIR", &dir);
        assert_eq!(socket_path(), None);
        std::fs::write(&socket, b"not a socket").unwrap();
        assert_eq!(socket_path(), None);
        std::fs::remove_file(&socket).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        assert_eq!(socket_path(), Some(socket.clone()));
        assert!(available());
        // A request against a listener that never answers is a reply error,
        // not a hang: the listener accepts and closes.
        let server = std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                drop(stream);
            }
        });
        let err = get(
            "https://example.invalid/",
            &[("accept", "text/xml")],
            Some(10),
            None,
        )
        .unwrap_err();
        assert!(matches!(err, Error::Io(_)), "{err}");
        server.join().unwrap();
        // A service that refuses after the head closes without reading the
        // body; the request write breaks on that, and what comes back is
        // the verdict, not the broken pipe.
        std::fs::remove_file(&socket).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let mut reader = BufReader::new(&stream);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(n) if n > 0 && line != "\n" => continue,
                        _ => break,
                    }
                }
                let verdict = format!("{PROTOCOL}\nerror refused: a body it will not read\n\n");
                let _ = (&stream).write_all(verdict.as_bytes());
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        });
        // Larger than a unix socket's send buffer, so the write is what
        // breaks; on a host whose buffer held it all the write would
        // succeed and the verdict would come back by the plainer arm.
        let body = vec![b'x'; 8 << 20];
        let err = post(
            "https://example.invalid/",
            &[("content-type", "text/plain")],
            &body,
            Some(10),
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::Refused(ref m) if m == "a body it will not read"),
            "{err}"
        );
        server.join().unwrap();
        std::env::remove_var("XDG_RUNTIME_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The frames read from `wire` until the stream is over, and the error
    /// that ended it if one did; a call past the end is checked to stay
    /// ended.
    fn drain(wire: &[u8], limit: u64) -> (Vec<Vec<u8>>, Option<Error>) {
        let mut reader = BufReader::new(wire);
        let mut frames = Frames::new(limit);
        let mut got = Vec::new();
        loop {
            let ended = match frames.next(&mut reader) {
                Ok(Some(bytes)) => {
                    got.push(bytes.to_vec());
                    continue;
                }
                Ok(None) => None,
                Err(e) => Some(e),
            };
            assert!(matches!(frames.next(&mut reader), Ok(None)));
            return (got, ended);
        }
    }

    #[test]
    fn a_stream_is_read_a_frame_at_a_time_to_its_end() {
        // What follows `end` is never read as the body.
        let (got, end) = drain(b"chunk 5\nhellochunk 3\n\nabend\nchunk 1\nx", DEFAULT_LIMIT);
        assert_eq!(got, [b"hello".to_vec(), b"\nab".to_vec()]);
        assert!(end.is_none(), "{end:?}");
        let (got, end) = drain(
            b"chunk 2\nhierror transport: the origin sent nothing for 120s\n",
            DEFAULT_LIMIT,
        );
        assert_eq!(got, [b"hi".to_vec()]);
        assert!(
            matches!(end, Some(Error::Transport(ref m)) if m == "the origin sent nothing for 120s"),
            "{end:?}"
        );
        let (_, end) = drain(b"error refused: response over 10 bytes\n", DEFAULT_LIMIT);
        assert!(
            matches!(end, Some(Error::Refused(ref m)) if m == "response over 10 bytes"),
            "{end:?}"
        );
        // A frame at the bound is taken whole, and a sum at the limit is
        // within it.
        let mut wire = format!("chunk {MAX_CHUNK}\n").into_bytes();
        wire.extend(vec![b'z'; MAX_CHUNK]);
        wire.extend_from_slice(b"end\n");
        let (got, end) = drain(&wire, MAX_CHUNK as u64);
        assert_eq!(got, [vec![b'z'; MAX_CHUNK]]);
        assert!(end.is_none(), "{end:?}");
    }

    #[test]
    fn a_stream_keeps_its_bounds_and_its_framing() {
        let long = format!("chunk {}", "1".repeat(MAX_LINE)).into_bytes();
        for (wire, limit, frames, reason) in [
            (b"chunk 0\n".to_vec(), DEFAULT_LIMIT, 0, "chunk \"0\""),
            (
                format!("chunk {}\n", MAX_CHUNK + 1).into_bytes(),
                DEFAULT_LIMIT,
                0,
                "chunk \"65537\"",
            ),
            (b"chunk x\n".to_vec(), DEFAULT_LIMIT, 0, "chunk \"x\""),
            (b"chunk -1\n".to_vec(), DEFAULT_LIMIT, 0, "chunk \"-1\""),
            (
                b"chunk +5\nhello".to_vec(),
                DEFAULT_LIMIT,
                0,
                "chunk \"+5\"",
            ),
            (
                b"chunk 05\nhello".to_vec(),
                DEFAULT_LIMIT,
                0,
                "chunk \"05\"",
            ),
            (
                b"chunk 4\nabcdchunk 4\nefgh".to_vec(),
                6,
                1,
                "the service sent 8 bytes past the 6 asked for",
            ),
            (b"chunk 5\nab".to_vec(), DEFAULT_LIMIT, 0, "read chunk: "),
            (
                b"chunk 2\nab".to_vec(),
                DEFAULT_LIMIT,
                1,
                "the service closed mid-stream",
            ),
            (b"body 3\n".to_vec(), DEFAULT_LIMIT, 0, "stream line"),
            (
                b"error bogus\n".to_vec(),
                DEFAULT_LIMIT,
                0,
                "error \"bogus\"",
            ),
            (long, DEFAULT_LIMIT, 0, "past the bound"),
        ] {
            let (got, end) = drain(&wire, limit);
            assert_eq!(got.len(), frames, "{reason}");
            assert!(
                matches!(end, Some(Error::Io(ref m)) if m.contains(reason)),
                "{reason}: {end:?}"
            );
        }
    }

    #[test]
    fn a_stream_head_and_a_counted_head_are_not_taken_for_each_other() {
        let mut reader = BufReader::new(
            &b"td-fetch 1\nstatus 200\nheader content-type: text/event-stream\nstream\n\nchunk 1\nx"[..],
        );
        let (status, headers) = read_stream_head(&mut reader).unwrap();
        assert_eq!(status, 200);
        assert_eq!(
            headers,
            [("content-type".to_string(), "text/event-stream".to_string())]
        );
        // The frames follow the head on the same reader.
        let mut frames = Frames::new(DEFAULT_LIMIT);
        assert_eq!(frames.next(&mut reader).unwrap(), Some(&b"x"[..]));
        let stream_head = |wire: &[u8]| read_stream_head(&mut BufReader::new(wire));
        assert!(matches!(
            stream_head(b"td-fetch 1\nstatus 200\nbody 0\n\n"),
            Err(Error::Io(ref m)) if m.contains("one body")
        ));
        assert!(matches!(
            stream_head(b"td-fetch 1\nstatus 200\n\n"),
            Err(Error::Io(ref m)) if m.contains("one body")
        ));
        assert!(matches!(
            stream_head(b"td-fetch 1\nstatus 200\nstream yes\n\n"),
            Err(Error::Io(ref m)) if m.contains("reply key")
        ));
        assert!(matches!(
            reply(b"td-fetch 1\nstatus 200\nstream\n\n"),
            Err(Error::Io(ref m)) if m.contains("streamed reply")
        ));
        assert!(matches!(
            reply(b"td-fetch 1\nstatus 200\nstream\nbody 0\n\n"),
            Err(Error::Io(ref m)) if m.contains("framed twice")
        ));
        // A refusal before the head is the same error either way.
        assert!(matches!(
            stream_head(b"td-fetch 1\nerror refused: loopback address\n\n"),
            Err(Error::Refused(ref m)) if m == "loopback address"
        ));
    }

    /// The request a client wrote, head lines to the blank one and the
    /// body it announced.
    fn read_request(reader: &mut impl BufRead) -> (String, Vec<u8>) {
        let mut head = String::new();
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\n" || line.is_empty() {
                break;
            }
            if let Some(count) = line.strip_prefix("body ") {
                length = count.trim_end().parse().unwrap();
            }
            head.push_str(&line);
        }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).unwrap();
        (head, body)
    }

    #[test]
    fn a_stream_is_asked_for_and_read_over_the_socket() {
        let _env = env_lock();
        let dir = std::env::temp_dir().join(format!("td-fetch-stream-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(SOCKET_DIRECTORY)).unwrap();
        let socket = dir.join(SOCKET_DIRECTORY).join(SOCKET_FILE);
        std::env::set_var("XDG_RUNTIME_DIR", &dir);
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let mut asked = Vec::new();
            for turn in 0..4 {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(&stream);
                asked.push(read_request(&mut reader));
                let mut out = &stream;
                match turn {
                    0 => {
                        let _ = out.write_all(b"td-fetch 1\nerror refused: scheme \"ftp\"\n\n");
                    }
                    1 => {
                        let _ = out.write_all(
                            b"td-fetch 1\nstatus 200\nheader content-type: text/event-stream\nstream\n\nchunk 6\ndata: ",
                        );
                        // The rest once the client holds the first frame:
                        // a frame is handed over as it comes.
                        let _ = wait.recv_timeout(Duration::from_secs(10));
                        let _ = out.write_all(b"chunk 4\nhi\n\nend\n");
                    }
                    2 => {
                        let _ = out.write_all(b"td-fetch 1\nstatus 200\nbody 2\n\nok");
                    }
                    _ => {
                        let _ = out.write_all(
                            b"td-fetch 1\nstatus 302\nheader location: /x\nstream\n\nend\n",
                        );
                    }
                }
            }
            asked
        });
        let err = post_stream("ftp://h/", &[], b"", None).unwrap_err();
        assert!(
            matches!(err, Error::Refused(ref m) if m == "scheme \"ftp\""),
            "{err}"
        );
        let url = "https://openrouter.ai/api/v1/chat/completions";
        let mut stream = post_stream(
            url,
            &[("content-type", "application/json")],
            b"{\"stream\":true}",
            Some(1024),
        )
        .unwrap();
        assert_eq!(stream.status, 200);
        assert_eq!(stream.header("content-type"), Some("text/event-stream"));
        assert_eq!(stream.next_chunk().unwrap(), Some(&b"data: "[..]));
        go.send(()).unwrap();
        assert_eq!(stream.next_chunk().unwrap(), Some(&b"hi\n\n"[..]));
        assert_eq!(stream.next_chunk().unwrap(), None);
        assert_eq!(stream.next_chunk().unwrap(), None);
        // The counted path's request is as it was; the streamed GET's
        // differs from it by the one line.
        let response = get("https://h/feed", &[], None, Some(0)).unwrap();
        assert_eq!(response.body, b"ok");
        let mut stream = get_stream("https://h/feed", &[], None, Some(0)).unwrap();
        assert_eq!(
            (stream.status, stream.header("location")),
            (302, Some("/x"))
        );
        assert_eq!(stream.next_chunk().unwrap(), None);
        let asked = server.join().unwrap();
        assert_eq!(
            asked[1],
            (
                format!(
                    "{PROTOCOL}\nmethod POST\nurl {url}\nheader content-type: application/json\nlimit 1024\nstream\nbody 15\n"
                ),
                b"{\"stream\":true}".to_vec()
            )
        );
        assert_eq!(
            asked[2].0,
            format!("{PROTOCOL}\nmethod GET\nurl https://h/feed\nlimit {DEFAULT_LIMIT}\nredirects 0\nbody 0\n")
        );
        assert_eq!(
            asked[3].0,
            format!("{PROTOCOL}\nmethod GET\nurl https://h/feed\nlimit {DEFAULT_LIMIT}\nredirects 0\nstream\nbody 0\n")
        );
        std::env::remove_var("XDG_RUNTIME_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
