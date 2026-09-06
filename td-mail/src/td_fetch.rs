//! The td fetch service, as a client: HTTP through the unix socket
//! `$XDG_RUNTIME_DIR/td-fetch/socket`, whose directory td-jail binds into a
//! jail that carries `sockets=fetch` (td's APPLICATIONS.md §W.8); the
//! directory rather than the socket inode, so a restarted service's fresh
//! socket is at the same path. The service holds the TLS trust, the
//! resolver, the timeouts and the body caps; this side holds a socket and
//! the framing, in `std` alone.
//!
//! One request per connection: a text head, a blank line, the body; back,
//! a text head, a blank line, the body, or an `error` line the service
//! wrote so a person can be told what happened. The service may answer a
//! bad head before the body is through, so the request is written whole
//! before anything is read, and a failed write is not the outcome: the
//! reply is.
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
/// The service answers within its budgets (a minute for the head, five for
/// the origin); this only bounds a service that has gone away mid-reply.
const REPLY_TIMEOUT: Duration = Duration::from_secs(420);

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

fn request(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    limit: Option<u64>,
    redirects: Option<u32>,
) -> Result<Response, Error> {
    let path = socket_path().ok_or_else(|| Error::Io("no td-fetch socket".into()))?;
    let mut stream = UnixStream::connect(&path).map_err(|e| Error::Io(format!("connect: {e}")))?;
    stream
        .set_read_timeout(Some(REPLY_TIMEOUT))
        .map_err(|e| Error::Io(e.to_string()))?;
    let mut head = format!("{PROTOCOL}\nmethod {method}\nurl {url}\n");
    for (name, value) in headers {
        if value.bytes().any(|b| b.is_ascii_control()) {
            return Err(Error::Malformed(format!(
                "header {name:?} carries a control byte"
            )));
        }
        head.push_str("header ");
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push('\n');
    }
    head.push_str(&format!("limit {}\n", limit.unwrap_or(DEFAULT_LIMIT)));
    if let Some(redirects) = redirects {
        head.push_str(&format!("redirects {redirects}\n"));
    }
    head.push_str(&format!("body {}\n\n", body.len()));
    // Written whole before anything is read; the service may already have
    // answered and closed, and then the reply is what matters, not EPIPE.
    let written = stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(body))
        .and_then(|()| stream.flush());
    let mut reader = BufReader::new(stream);
    let reply = read_reply(&mut reader);
    match (reply, written) {
        (Ok(response), _) => Ok(response),
        (Err(error), Ok(())) => Err(error),
        (Err(_), Err(write_error)) => Err(Error::Io(format!("write: {write_error}"))),
    }
}

fn read_line(reader: &mut impl BufRead) -> Result<String, Error> {
    let mut raw = Vec::with_capacity(128);
    let read = reader
        .by_ref()
        .take(MAX_LINE as u64)
        .read_until(b'\n', &mut raw)
        .map_err(|e| Error::Io(format!("read: {e}")))?;
    if read == 0 {
        return Err(Error::Io("the service closed without a reply".into()));
    }
    if raw.last() != Some(&b'\n') {
        return Err(Error::Io("a reply line past the bound".into()));
    }
    raw.pop();
    String::from_utf8(raw).map_err(|_| Error::Io("a reply line that is not UTF-8".into()))
}

fn read_reply(reader: &mut impl BufRead) -> Result<Response, Error> {
    let first = read_line(reader)?;
    if first != PROTOCOL {
        return Err(Error::Io(format!(
            "the reply began {first:?}, not {PROTOCOL:?}"
        )));
    }
    let mut status = None;
    let mut headers = Vec::new();
    let mut body_len = 0u64;
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
            "body" => {
                body_len = value
                    .parse::<u64>()
                    .map_err(|_| Error::Io(format!("body {value:?}")))?;
            }
            "error" => {
                let (kind, reason) = value.split_once(": ").unwrap_or((value, ""));
                return Err(match kind {
                    "refused" => Error::Refused(reason.to_string()),
                    "malformed" => Error::Malformed(reason.to_string()),
                    "transport" => Error::Transport(reason.to_string()),
                    _ => Error::Io(format!("error {value:?}")),
                });
            }
            other => return Err(Error::Io(format!("reply key {other:?}"))),
        }
    }
    let status = status.ok_or_else(|| Error::Io("a reply with no status".into()))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(bytes: &[u8]) -> Result<Response, Error> {
        read_reply(&mut BufReader::new(bytes))
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
        let dir = std::env::temp_dir().join(format!("td-fetch-client-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(SOCKET_DIRECTORY)).unwrap();
        let socket = dir.join(SOCKET_DIRECTORY).join(SOCKET_FILE);
        // This test owns the variable for its duration; the crate's other
        // tests do not read it.
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
        std::env::remove_var("XDG_RUNTIME_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
