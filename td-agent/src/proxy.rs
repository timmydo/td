//! The in-jail end of the network proxy (DESIGN.md §10): in an instance
//! whose workspace's policy is not `off`, the tool host listens on
//! `127.0.0.1:PORT` in the instance's own network namespace and speaks
//! HTTP `CONNECT` and plain-HTTP proxying there. Each connection it
//! accepts becomes a link over the instance's pipe (`host::Link`): its
//! destination goes up in an `Open`, td-agent decides it and opens it
//! through the egress relay, and the bytes go both ways in `Bytes`
//! frames, each direction at most `LINK_WINDOW` ahead of the other
//! end's `Took`, so one slow reader never stalls the pipe. Nothing here
//! decides a destination but a loopback one, which it refuses itself;
//! everything else is td-agent's.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::host::{Credit, Link, Up, LINK_CHUNK, MAX_LINKS};

/// The port the proxy listens on, in the instance's own namespace.
pub const PORT: u16 = 3128;
/// The longest request head taken.
const MAX_HEAD: usize = 16 * 1024;
/// How long a client has to send its head.
const HEAD_TIME: Duration = Duration::from_secs(30);
/// How long a link waits for td-agent to open or refuse it, a person
/// deciding included (§11).
const OPEN_TIME: Duration = Duration::from_secs(30 * 60);
/// How long a write to the client may wait.
const WRITE_TIME: Duration = Duration::from_secs(5 * 60);

/// The proxy's links, by id, and where it sends its frames, until it
/// is stopped.
pub struct Proxy {
    links: Mutex<BTreeMap<u64, Arc<End>>>,
    next: AtomicU64,
    outbox: Mutex<Option<SyncSender<Up>>>,
}

/// One link's end in the tool host: what td-agent sends it, and how far
/// ahead it may send.
struct End {
    events: Mutex<Sender<Event>>,
    credit: Credit,
}

impl End {
    /// Lets go of whatever of this link waits: its sender for room, its
    /// deliverer for td-agent.
    fn wake(&self) {
        self.credit.close();
        if let Ok(events) = self.events.lock() {
            let _ = events.send(Event::Shut);
        }
    }
}

/// What td-agent says of a link.
enum Event {
    Opened,
    Refused(String),
    Bytes(Vec<u8>),
    Shut,
}

/// Listens on `port` of loopback, `PORT` but in tests, and serves each
/// connection as a link, sending its frames to `outbox`; the port bound.
pub fn start(outbox: SyncSender<Up>, port: u16) -> Result<(Arc<Proxy>, u16), String> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| format!("the network proxy on 127.0.0.1:{port}: {e}"))?;
    let bound = listener
        .local_addr()
        .map_err(|e| format!("the network proxy: {e}"))?
        .port();
    let proxy = Arc::new(Proxy {
        links: Mutex::new(BTreeMap::new()),
        next: AtomicU64::new(1),
        outbox: Mutex::new(Some(outbox)),
    });
    let serving = Arc::clone(&proxy);
    std::thread::Builder::new()
        .name("proxy".into())
        .spawn(move || serving.accept(listener))
        .map_err(|e| format!("the network proxy: {e}"))?;
    Ok((proxy, bound))
}

impl Proxy {
    fn accept(self: Arc<Self>, listener: TcpListener) {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                continue;
            };
            let proxy = Arc::clone(&self);
            let _ = std::thread::Builder::new()
                .name("proxy-link".into())
                .spawn(move || proxy.serve(stream));
        }
    }

    /// What td-agent says of a link, from the tool host's input; the
    /// wrong direction's, or a link no longer open, is nothing.
    pub fn down(&self, link: Link) {
        let id = link.id();
        let Some(end) = self.links.lock().ok().and_then(|l| l.get(&id).cloned()) else {
            return;
        };
        let event = match link {
            Link::Took { bytes, .. } => {
                end.credit.give(bytes);
                return;
            }
            Link::Opened { .. } => Event::Opened,
            Link::Refused { why, .. } => Event::Refused(why),
            Link::Bytes { data, .. } => Event::Bytes(data),
            Link::Shut { .. } => {
                end.credit.close();
                Event::Shut
            }
            Link::Open { .. } => return,
        };
        let Ok(events) = end.events.lock() else {
            return;
        };
        let _ = events.send(event);
    }

    fn send(&self, link: Link) -> bool {
        let outbox = self.outbox.lock().ok().and_then(|outbox| outbox.clone());
        outbox.is_some_and(|outbox| outbox.send(Up::Link(link)).is_ok())
    }

    /// Sends nothing more, and ends every link: the tool host's input
    /// ended, and its writer waits for every sender to go.
    pub fn stop(&self) {
        if let Ok(mut outbox) = self.outbox.lock() {
            *outbox = None;
        }
        let ends: Vec<Arc<End>> = self
            .links
            .lock()
            .map(|links| links.values().cloned().collect())
            .unwrap_or_default();
        for end in ends {
            end.wake();
        }
    }

    /// A link taken in, if there is room for one.
    fn register(&self) -> Option<(u64, Arc<End>, Receiver<Event>)> {
        let mut links = self.links.lock().ok()?;
        if links.len() >= MAX_LINKS {
            return None;
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (events, received) = mpsc::channel();
        let end = Arc::new(End {
            events: Mutex::new(events),
            credit: Credit::default(),
        });
        links.insert(id, Arc::clone(&end));
        Some((id, end, received))
    }

    fn forget(&self, id: u64) {
        if let Ok(mut links) = self.links.lock() {
            links.remove(&id);
        }
    }

    /// One connection: its request read, its destination opened through
    /// td-agent or refused, and then its bytes carried both ways.
    fn serve(self: Arc<Self>, mut stream: TcpStream) {
        let request = match read_request(&mut stream) {
            Ok(request) => request,
            Err((status, why)) => {
                let _ = answer(&mut stream, status, &why);
                return;
            }
        };
        let Some((id, end, events)) = self.register() else {
            let why = format!("{MAX_LINKS} connections are already open through the proxy");
            let _ = answer(&mut stream, 503, &why);
            return;
        };
        let opened = self.send(Link::Open {
            link: id,
            host: request.host.clone(),
            port: request.port,
        });
        let said = if opened {
            events.recv_timeout(OPEN_TIME).ok()
        } else {
            None
        };
        match said {
            Some(Event::Opened) => {}
            Some(Event::Refused(why)) => {
                self.forget(id);
                let _ = answer(&mut stream, 403, &why);
                return;
            }
            _ => {
                self.forget(id);
                self.send(Link::Shut { link: id });
                let _ = answer(&mut stream, 504, "td-agent did not open the connection");
                return;
            }
        }
        if request.connect {
            let _ = stream.set_write_timeout(Some(WRITE_TIME));
            if stream
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .is_err()
            {
                self.end(id, &end, &stream);
                return;
            }
        }
        let carried = request
            .forward
            .chunks(LINK_CHUNK)
            .all(|chunk| self.carry(id, &end, chunk.to_vec()));
        let Ok(reading) = stream.try_clone() else {
            self.end(id, &end, &stream);
            return;
        };
        if carried {
            let proxy = Arc::clone(&self);
            let pumping = Arc::clone(&end);
            let left = request.left;
            let _ = std::thread::Builder::new()
                .name("proxy-up".into())
                .spawn(move || proxy.pump(id, &pumping, reading, left));
        }
        if carried {
            self.deliver(id, &events, &mut stream);
        }
        self.end(id, &end, &stream);
    }

    /// Sends `data` up as link `id`'s, once there is room for it.
    fn carry(&self, id: u64, end: &End, data: Vec<u8>) -> bool {
        let n = u64::try_from(data.len()).unwrap_or(u64::MAX);
        end.credit.take(n) && self.send(Link::Bytes { link: id, data })
    }

    /// The client's bytes, up, until it ends, or, for a plain request,
    /// until `left` of its body have gone, anything past them another
    /// request this connection does not carry, read only to see the
    /// client end; a client that ends ends the link, its deliverer woken.
    fn pump(&self, id: u64, end: &End, mut reading: TcpStream, mut left: Option<u64>) {
        let mut buffer = vec![0u8; LINK_CHUNK];
        loop {
            let (want, carrying) = match left {
                Some(0) => (LINK_CHUNK, false),
                Some(left) => (
                    usize::try_from(left).map_or(LINK_CHUNK, |l| l.min(LINK_CHUNK)),
                    true,
                ),
                None => (LINK_CHUNK, true),
            };
            let read = match reading.read(buffer.get_mut(..want).unwrap_or_default()) {
                Ok(0) => break,
                Ok(read) => read,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            if !carrying {
                continue;
            }
            if let Some(left) = left.as_mut() {
                *left = left.saturating_sub(u64::try_from(read).unwrap_or(u64::MAX));
            }
            let data = buffer.get(..read).unwrap_or_default().to_vec();
            if !self.carry(id, end, data) {
                break;
            }
        }
        self.send(Link::Shut { link: id });
        end.wake();
        let _ = reading.shutdown(Shutdown::Both);
    }

    /// td-agent's bytes, down to the client, each taken back as written,
    /// until either end shuts.
    fn deliver(&self, id: u64, events: &Receiver<Event>, stream: &mut TcpStream) {
        let _ = stream.set_write_timeout(Some(WRITE_TIME));
        while let Ok(event) = events.recv() {
            let Event::Bytes(data) = event else {
                break;
            };
            if stream.write_all(&data).is_err() {
                break;
            }
            let bytes = u64::try_from(data.len()).unwrap_or(u64::MAX);
            if !self.send(Link::Took { link: id, bytes }) {
                break;
            }
        }
    }

    /// Link `id` ended here: td-agent told before its place is given
    /// up, so a new link never finds td-agent still counting this one.
    fn end(&self, id: u64, end: &End, stream: &TcpStream) {
        self.send(Link::Shut { link: id });
        end.credit.close();
        self.forget(id);
        let _ = stream.shutdown(Shutdown::Both);
    }
}

/// A request the proxy carries: where to, whether it is a tunnel, and
/// the bytes to send once open (a plain request's head, rewritten, and
/// whatever the client sent past its head).
#[derive(Debug, PartialEq, Eq)]
struct Request {
    host: String,
    port: u16,
    connect: bool,
    forward: Vec<u8>,
    /// For a plain request, how much of its body is still to come past
    /// `forward`; none for a tunnel, which carries whatever comes.
    left: Option<u64>,
}

/// The request's head, read up to its blank line within `HEAD_TIME`, and
/// what it asks; a refusal is a status and why.
fn read_request(stream: &mut TcpStream) -> Result<Request, (u16, String)> {
    let _ = stream.set_read_timeout(Some(HEAD_TIME));
    let mut head = Vec::new();
    let mut buffer = [0u8; 4096];
    let end = loop {
        if let Some(at) = find(&head, b"\r\n\r\n") {
            break at + 4;
        }
        if head.len() > MAX_HEAD {
            return Err((431, format!("a request head is at most {MAX_HEAD} bytes")));
        }
        match stream.read(&mut buffer) {
            Ok(0) => return Err((400, "the request ended inside its head".into())),
            Ok(read) => head.extend_from_slice(buffer.get(..read).unwrap_or_default()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err((408, format!("reading the request: {e}"))),
        }
    };
    let _ = stream.set_read_timeout(None);
    let rest = head.split_off(end);
    parse_request(&head, rest)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// What a head asks: `CONNECT host:port` for a tunnel, or an absolute
/// `http://` URL, rewritten to its path with `Proxy-` headers dropped.
fn parse_request(head: &[u8], rest: Vec<u8>) -> Result<Request, (u16, String)> {
    let text = std::str::from_utf8(head).map_err(|_| (400, "the head is not text".to_string()))?;
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap_or_default();
    let mut words = first.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (words.next(), words.next(), words.next(), words.next())
    else {
        return Err((400, format!("{first:?} is not a request line")));
    };
    if !version.starts_with("HTTP/1.") {
        return Err((505, format!("{version} is not HTTP/1")));
    }
    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = authority(target, None)?;
        return Ok(Request {
            host,
            port,
            connect: true,
            forward: rest,
            left: None,
        });
    }
    let after = target
        .get(..7)
        .filter(|scheme| scheme.eq_ignore_ascii_case("http://"))
        .and_then(|_| target.get(7..))
        .ok_or_else(|| {
            (
                400,
                format!("{target:?} is neither CONNECT's host:port nor an http:// URL"),
            )
        })?;
    let at = after.find(['/', '?']).unwrap_or(after.len());
    let authority_text = after.get(..at).unwrap_or_default();
    let path = match after.get(at..).unwrap_or_default() {
        "" => "/".to_string(),
        path if path.starts_with('/') => path.to_string(),
        query => format!("/{query}"),
    };
    let (host, port) = authority(authority_text, Some(80))?;
    // One request per connection: the destination judged is the one
    // whose URL this is, so the `Host` is its authority, the origin is
    // told to close, and a body is carried only as long as it says.
    let mut forward =
        format!("{method} {path} {version}\r\nHost: {authority_text}\r\n").into_bytes();
    let mut length: Option<u64> = None;
    for line in lines.filter(|line| !line.is_empty()) {
        // A folded line, or a name the origin might read otherwise, is
        // refused rather than forwarded as this proxy did not read it.
        let Some((name, value)) = line
            .split_once(':')
            .filter(|(name, _)| !name.is_empty() && name.bytes().all(|b| b.is_ascii_graphic()))
        else {
            return Err((400, "a header line that is not a name and a value".into()));
        };
        let name = name.to_ascii_lowercase();
        if name.starts_with("proxy-")
            || ["host", "connection", "keep-alive"].contains(&name.as_str())
        {
            continue;
        }
        if name == "transfer-encoding" {
            return Err((
                501,
                "a request body of no stated length is not carried; send it with Content-Length"
                    .into(),
            ));
        }
        if name == "content-length" {
            let value = value.trim();
            let stated = Some(value)
                .filter(|v| !v.is_empty() && v.len() <= 19 && v.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or_else(|| (400, format!("{value:?} is not a length")))?;
            if length.is_some_and(|length| length != stated) {
                return Err((400, "two lengths".into()));
            }
            length = Some(stated);
        }
        forward.extend_from_slice(line.as_bytes());
        forward.extend_from_slice(b"\r\n");
    }
    forward.extend_from_slice(b"Connection: close\r\n\r\n");
    let body = length.unwrap_or(0);
    let now = rest.len().min(usize::try_from(body).unwrap_or(usize::MAX));
    forward.extend_from_slice(rest.get(..now).unwrap_or_default());
    Ok(Request {
        host,
        port,
        connect: false,
        forward,
        left: Some(body.saturating_sub(u64::try_from(now).unwrap_or(u64::MAX))),
    })
}

/// `host:port`, `[v6]:port`, or, given a `default`, either without its
/// port; a loopback or unspecified host is refused here, since no
/// destination of the jail's own namespace is the network's.
fn authority(text: &str, default: Option<u16>) -> Result<(String, u16), (u16, String)> {
    let wrong = || (400, format!("{text:?} is not a host and port"));
    let (host, port) = if let Some(rest) = text.strip_prefix('[') {
        let (inner, after) = rest.split_once(']').ok_or_else(wrong)?;
        let port = match after {
            "" => None,
            after => Some(after.strip_prefix(':').ok_or_else(wrong)?),
        };
        (inner, port)
    } else {
        match text.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (text, None),
        }
    };
    let port = match port {
        Some(port) => port
            .parse::<u16>()
            .ok()
            .filter(|p| *p != 0)
            .ok_or_else(wrong)?,
        None => default.ok_or_else(wrong)?,
    };
    if host.is_empty() {
        return Err(wrong());
    }
    let lower = host.trim_end_matches('.').to_ascii_lowercase();
    let local = lower == "localhost"
        || lower.ends_with(".localhost")
        || lower
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified());
    if local {
        return Err((
            403,
            format!("{host} is this jail's own: the proxy reaches the network, not loopback"),
        ));
    }
    Ok((host.to_string(), port))
}

/// Answers a request the proxy will not carry, and ends it.
fn answer(stream: &mut TcpStream, status: u16, why: &str) -> io::Result<()> {
    let reason = match status {
        400 => "Bad Request",
        403 => "Forbidden",
        408 => "Request Timeout",
        431 => "Request Header Fields Too Large",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        _ => "Error",
    };
    let body = format!("td-agent: {why}\n");
    let _ = stream.set_write_timeout(Some(HEAD_TIME));
    stream.write_all(
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    )?;
    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}

/// The environment that points a command's programs at the proxy:
/// `http_proxy`, `https_proxy` and `all_proxy`, lower case and upper, and
/// `no_proxy` naming loopback.
pub fn environment() -> Vec<(String, String)> {
    let url = format!("http://127.0.0.1:{PORT}");
    let mut env = Vec::new();
    for name in ["http_proxy", "https_proxy", "all_proxy"] {
        env.push((name.to_string(), url.clone()));
        env.push((name.to_ascii_uppercase(), url.clone()));
    }
    for name in ["no_proxy", "NO_PROXY"] {
        env.push((name.to_string(), "localhost,127.0.0.1,::1".to_string()));
    }
    env
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use crate::host::LINK_WINDOW;

    fn request(head: &str, rest: &[u8]) -> Result<Request, (u16, String)> {
        parse_request(head.as_bytes(), rest.to_vec())
    }

    /// `CONNECT` names a host and port, which it must; a plain request's
    /// absolute URL is rewritten to its path, its `Proxy-` headers
    /// dropped; what follows the head goes with it; anything else, and a
    /// loopback destination, is refused with its status.
    #[test]
    fn requests_are_read_and_rewritten() {
        assert_eq!(
            request(
                "CONNECT static.crates.io:443 HTTP/1.1\r\nHost: x\r\n\r\n",
                b"\x16"
            ),
            Ok(Request {
                host: "static.crates.io".into(),
                port: 443,
                connect: true,
                forward: b"\x16".to_vec(),
                left: None,
            })
        );
        assert_eq!(
            request("CONNECT [2606:4700::1111]:8443 HTTP/1.0\r\n\r\n", b""),
            Ok(Request {
                host: "2606:4700::1111".into(),
                port: 8443,
                connect: true,
                forward: Vec::new(),
                left: None,
            })
        );
        // The Host is the URL's, the origin told to close, Proxy-,
        // Connection and Keep-Alive headers dropped, and the body carried
        // as far as its length: what follows is another request.
        assert_eq!(
            request(
                "POST http://Example.test:8080/a?b=c HTTP/1.1\r\nHost: elsewhere.test\r\nProxy-Connection: keep-alive\r\nproxy-authorization: x\r\nConnection: keep-alive\r\nKeep-Alive: 5\r\nContent-Length: 4\r\nAccept: */*\r\n\r\n",
                b"bodyGET http://other.test/ HTTP/1.1\r\n\r\n"
            ),
            Ok(Request {
                host: "Example.test".into(),
                port: 8080,
                connect: false,
                forward: b"POST /a?b=c HTTP/1.1\r\nHost: Example.test:8080\r\nContent-Length: 4\r\nAccept: */*\r\nConnection: close\r\n\r\nbody".to_vec(),
                left: Some(0),
            })
        );
        assert_eq!(
            request(
                "PUT http://example.test/up HTTP/1.1\r\nContent-Length: 10\r\n\r\n",
                b"abc"
            )
            .map(|r| r.left),
            Ok(Some(7))
        );
        assert_eq!(
            request("HEAD http://example.test HTTP/1.1\r\n\r\n", b"")
                .map(|r| (r.port, r.forward, r.left)),
            Ok((
                80,
                b"HEAD / HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n".to_vec(),
                Some(0)
            ))
        );
        assert_eq!(
            request("GET http://example.test?q=1 HTTP/1.1\r\n\r\n", b"")
                .map(|r| (r.host, r.forward)),
            Ok((
                "example.test".into(),
                b"GET /?q=1 HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n".to_vec()
            ))
        );
        for (head, status) in [
            ("CONNECT example.test HTTP/1.1\r\n\r\n", 400),
            ("CONNECT example.test:0 HTTP/1.1\r\n\r\n", 400),
            ("CONNECT :443 HTTP/1.1\r\n\r\n", 400),
            ("GET https://example.test/ HTTP/1.1\r\n\r\n", 400),
            ("GET /relative HTTP/1.1\r\n\r\n", 400),
            ("GET http://example.test/ HTTP/2\r\n\r\n", 505),
            ("GET http://example.test/\r\n\r\n", 400),
            ("POST http://example.test/ HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n", 501),
            ("POST http://example.test/ HTTP/1.1\r\nContent-Length: x\r\n\r\n", 400),
            ("POST http://example.test/ HTTP/1.1\r\nContent-Length: +1\r\n\r\n", 400),
            ("POST http://example.test/ HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n", 400),
            ("GET http://example.test/ HTTP/1.1\r\nAccept: a\r\n b\r\n\r\n", 400),
            ("GET http://example.test/ HTTP/1.1\r\n\tb\r\n\r\n", 400),
            ("POST http://example.test/ HTTP/1.1\r\nContent-Length : 5\r\n\r\n", 400),
            ("GET http://example.test/ HTTP/1.1\r\nno colon\r\n\r\n", 400),
            ("GET http://example.test/ HTTP/1.1\r\n: empty\r\n\r\n", 400),
            ("CONNECT localhost:22 HTTP/1.1\r\n\r\n", 403),
            ("CONNECT LOCALHOST.:22 HTTP/1.1\r\n\r\n", 403),
            ("CONNECT db.localhost:5432 HTTP/1.1\r\n\r\n", 403),
            ("CONNECT 127.0.0.2:22 HTTP/1.1\r\n\r\n", 403),
            ("CONNECT [::1]:22 HTTP/1.1\r\n\r\n", 403),
            ("CONNECT 0.0.0.0:22 HTTP/1.1\r\n\r\n", 403),
            ("GET http://127.0.0.1:9/ HTTP/1.1\r\n\r\n", 403),
        ] {
            assert_eq!(
                request(head, b"").map_err(|e| e.0),
                Err(status),
                "{head:?}"
            );
        }
    }

    /// A client that ends its tunnel frees its place at once, so more
    /// than `MAX_LINKS` tunnels opened and closed in turn are all taken;
    /// stopped, the proxy lets go of every sender of its frames.
    #[test]
    fn a_closed_tunnel_frees_its_place_and_a_stopped_proxy_its_frames() {
        let (proxy, port, frames) = proxy();
        for _ in 0..MAX_LINKS + 4 {
            let mut stream = client(port, "CONNECT example.test:443 HTTP/1.1\r\n\r\n");
            let Link::Open { link, .. } = next(&frames) else {
                panic!("no open");
            };
            proxy.down(Link::Opened { link });
            line(&mut stream);
            drop(stream);
            assert_eq!(next(&frames), Link::Shut { link });
            // Its deliverer ends too, giving the place back.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while proxy.links.lock().unwrap().contains_key(&link) {
                assert!(std::time::Instant::now() < deadline, "link {link} kept");
                std::thread::sleep(Duration::from_millis(10));
            }
            while let Ok(Up::Link(Link::Shut { .. })) = frames.try_recv() {}
        }
        let mut open = client(port, "CONNECT example.test:443 HTTP/1.1\r\n\r\n");
        let Link::Open { link, .. } = next(&frames) else {
            panic!("no open");
        };
        proxy.down(Link::Opened { link });
        line(&mut open);
        proxy.stop();
        let mut rest = Vec::new();
        let _ = open.read_to_end(&mut rest);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match frames.recv_timeout(Duration::from_millis(100)) {
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                _ => assert!(std::time::Instant::now() < deadline, "a sender kept"),
            }
        }
    }

    /// A plain request's connection carries that request alone: a second
    /// one pipelined behind it never goes up.
    #[test]
    fn a_plain_connection_carries_one_request() {
        let (proxy, port, frames) = proxy();
        let mut stream = client(
            port,
            "GET http://a.test/ HTTP/1.1\r\nHost: a.test\r\n\r\nGET http://b.test/ HTTP/1.1\r\nHost: b.test\r\nAuthorization: secret\r\n\r\n",
        );
        let Link::Open { link, host, .. } = next(&frames) else {
            panic!("no open");
        };
        assert_eq!(host, "a.test");
        proxy.down(Link::Opened { link });
        assert_eq!(
            next(&frames),
            Link::Bytes {
                link,
                data: b"GET / HTTP/1.1\r\nHost: a.test\r\nConnection: close\r\n\r\n".to_vec()
            }
        );
        assert!(frames.recv_timeout(Duration::from_millis(300)).is_err());
        // Nor one sent once the first is under way.
        stream
            .write_all(b"GET http://c.test/ HTTP/1.1\r\nHost: c.test\r\n\r\n")
            .unwrap();
        assert!(frames.recv_timeout(Duration::from_millis(300)).is_err());
        proxy.down(Link::Bytes {
            link,
            data: b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec(),
        });
        proxy.down(Link::Shut { link });
        let mut said = String::new();
        stream.read_to_string(&mut said).unwrap();
        assert_eq!(said, "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n");
    }

    /// A plain request's client that leaves ends its link though its
    /// request had no body to read.
    #[test]
    fn a_plain_client_that_leaves_ends_its_link() {
        let (proxy, port, frames) = proxy();
        let stream = client(port, "GET http://a.test/ HTTP/1.1\r\n\r\n");
        let Link::Open { link, .. } = next(&frames) else {
            panic!("no open");
        };
        proxy.down(Link::Opened { link });
        let Link::Bytes { .. } = next(&frames) else {
            panic!("no request");
        };
        drop(stream);
        assert_eq!(next(&frames), Link::Shut { link });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while proxy.links.lock().unwrap().contains_key(&link) {
            assert!(std::time::Instant::now() < deadline, "link {link} kept");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn the_environment_names_the_proxy_both_cases() {
        let env = environment();
        for name in ["http_proxy", "HTTPS_PROXY", "all_proxy", "ALL_PROXY"] {
            assert!(
                env.contains(&(name.to_string(), format!("http://127.0.0.1:{PORT}"))),
                "{name}"
            );
        }
        assert!(env.contains(&("NO_PROXY".into(), "localhost,127.0.0.1,::1".into())));
    }

    /// A proxy on a port of its own, and the frames it sends.
    fn proxy() -> (Arc<Proxy>, u16, mpsc::Receiver<Up>) {
        let (outbox, frames) = mpsc::sync_channel(1024);
        let (proxy, port) = start(outbox, 0).unwrap();
        (proxy, port, frames)
    }

    fn next(frames: &mpsc::Receiver<Up>) -> Link {
        match frames.recv_timeout(Duration::from_secs(10)).unwrap() {
            Up::Link(link) => link,
            other => panic!("{other:?}"),
        }
    }

    fn client(port: u16, head: &str) -> TcpStream {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream.write_all(head.as_bytes()).unwrap();
        stream
    }

    fn line(stream: &mut TcpStream) -> String {
        let mut got = Vec::new();
        let mut byte = [0u8; 1];
        while !got.ends_with(b"\r\n\r\n") {
            assert_eq!(stream.read(&mut byte).unwrap(), 1);
            got.extend_from_slice(&byte);
        }
        String::from_utf8(got).unwrap()
    }

    /// A tunnel: its destination goes up, it opens once td-agent says
    /// so, bytes go both ways, each delivered is taken back, and the
    /// client's end shuts the link.
    #[test]
    fn a_tunnel_is_opened_by_td_agent_and_carries_both_ways() {
        let (proxy, port, frames) = proxy();
        let mut stream = client(port, "CONNECT example.test:443 HTTP/1.1\r\n\r\n");
        let Link::Open {
            link,
            host,
            port: to,
        } = next(&frames)
        else {
            panic!("no open");
        };
        assert_eq!((host.as_str(), to), ("example.test", 443));
        proxy.down(Link::Opened { link });
        assert_eq!(
            line(&mut stream),
            "HTTP/1.1 200 Connection established\r\n\r\n"
        );
        stream.write_all(b"ping").unwrap();
        assert_eq!(
            next(&frames),
            Link::Bytes {
                link,
                data: b"ping".to_vec()
            }
        );
        proxy.down(Link::Bytes {
            link,
            data: b"pong".to_vec(),
        });
        let mut got = [0u8; 4];
        stream.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"pong");
        assert_eq!(next(&frames), Link::Took { link, bytes: 4 });
        drop(stream);
        assert_eq!(next(&frames), Link::Shut { link });
    }

    /// A plain request goes up rewritten once open; a refused one is
    /// answered 403 with td-agent's reason; td-agent's end shuts the
    /// client's.
    #[test]
    fn a_plain_request_is_rewritten_and_a_refused_one_answered() {
        let (proxy, port, frames) = proxy();
        let mut refused = client(port, "CONNECT other.test:443 HTTP/1.1\r\n\r\n");
        let Link::Open { link, .. } = next(&frames) else {
            panic!("no open");
        };
        proxy.down(Link::Refused {
            link,
            why: "other.test is not on this workspace's allowlist".into(),
        });
        let mut said = String::new();
        refused.read_to_string(&mut said).unwrap();
        assert!(said.starts_with("HTTP/1.1 403 Forbidden\r\n"), "{said}");
        assert!(said.ends_with("td-agent: other.test is not on this workspace's allowlist\n"));
        let mut stream = client(
            port,
            "GET http://example.test/x HTTP/1.1\r\nHost: example.test\r\nProxy-Connection: keep-alive\r\n\r\n",
        );
        let Link::Open { link, port: to, .. } = next(&frames) else {
            panic!("no open");
        };
        assert_eq!(to, 80);
        proxy.down(Link::Opened { link });
        assert_eq!(
            next(&frames),
            Link::Bytes {
                link,
                data: b"GET /x HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n"
                    .to_vec()
            }
        );
        proxy.down(Link::Shut { link });
        let mut rest = Vec::new();
        assert_eq!(stream.read_to_end(&mut rest).unwrap(), 0);
        // A loopback destination never goes up.
        let mut local = client(port, "CONNECT localhost:22 HTTP/1.1\r\n\r\n");
        let mut said = String::new();
        local.read_to_string(&mut said).unwrap();
        assert!(said.starts_with("HTTP/1.1 403"), "{said}");
        assert!(matches!(
            frames.recv_timeout(Duration::from_millis(300)),
            Err(_) | Ok(Up::Link(Link::Shut { .. }))
        ));
    }

    /// The client's bytes go up no more than the window ahead of
    /// td-agent's `Took`.
    #[test]
    fn bytes_go_up_no_further_than_the_window() {
        let (proxy, port, frames) = proxy();
        let mut stream = client(port, "CONNECT example.test:443 HTTP/1.1\r\n\r\n");
        let Link::Open { link, .. } = next(&frames) else {
            panic!("no open");
        };
        proxy.down(Link::Opened { link });
        line(&mut stream);
        let mut writer = stream.try_clone().unwrap();
        std::thread::spawn(move || {
            let _ = writer.write_all(&vec![7u8; 2 * LINK_WINDOW as usize]);
        });
        // Until the window holds the rest back.
        let mut up = 0u64;
        while let Ok(Up::Link(Link::Bytes { data, .. })) =
            frames.recv_timeout(Duration::from_millis(500))
        {
            up += data.len() as u64;
        }
        assert!(up <= LINK_WINDOW, "{up}");
        assert!(up > LINK_WINDOW - LINK_CHUNK as u64, "{up}");
        proxy.down(Link::Took { link, bytes: up });
        assert!(matches!(next(&frames), Link::Bytes { .. }));
    }
}
