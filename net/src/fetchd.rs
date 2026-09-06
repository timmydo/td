// td-fetchd — the target's one network client: an HTTP(S) fetch service for the
// jailed terminal applications, served over a unix socket in a directory of its
// own, `td-fetch/socket` under the runtime directory, which td-jail binds as a
// directory into a jail that carries `sockets=fetch` (APPLICATIONS.md §W.8), so
// a restarted service's fresh socket is the one a running jail connects to. The
// applications hold no TCP socket, resolve no names and carry no trust store;
// this applet does, once, with the tier's reviewed closure.
//
// One request per connection. The client writes a text head, a blank line and
// the body it announced; the service answers with a text head, a blank line
// and the body it announced, or an `error` head with no body. Every line is
// bounded, every body is bounded, and the reply names the refusal, so a
// client can show a person what happened rather than a closed socket.
//
//   td-fetch 1                     td-fetch 1
//   method GET                     status 200
//   url https://host/path          header content-type: text/xml
//   header accept: text/xml        body 1234
//   limit 16777216                 (blank line, then 1234 bytes)
//   body 0
//   (blank line)
//
// Not a proxy: the service refuses any scheme but http and https and any host
// that names or resolves to a loopback, unspecified, link-local, broadcast or
// multicast address, because td's own loopback listeners live inside the
// shared stack and the grant must not reach them. The address policy sits in
// the resolver ureq itself calls before every connection, the redirected ones
// included, so the destination checked is the destination connected to; no
// URL is parsed twice by two parsers that could disagree. It asks for identity
// encoding since the tier's closure has no decoder; follows up to five
// redirects for GET, fewer when the client's `redirects N` asks, none for
// POST, dropping `authorization` and `cookie` on the way as ureq does, so a
// client that must carry a credential across a redirect asks for none and
// follows the `location` it is handed itself, a relative one joined against
// its URL; and gives the whole exchange with the origin five minutes over
// `http.rs`'s connect deadline. What it does not do is decide destinations:
// an application with the grant reaches any other host, as `shared=network`
// let it, a listener on the machine's own routable address among them, since
// a crate without `unsafe` cannot enumerate the interfaces; a name that
// answers the resolver one way now and another way later is defended only as
// far as the address it returned is the one connected to; and a name lookup
// is outside every deadline, ureq's resolver being a blocking call (all four
// are §W.8's recorded gaps).
//
// Authentication is the socket's: `/run/user/<uid>` is mode 0700 and the
// socket 0600, so only that uid connects. This crate forbids `unsafe`, so
// peer credentials are not read; the mode is the whole of it.
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// The first line of every head, both ways. A client speaking anything else
/// gets `error malformed` and nothing more.
pub(crate) const PROTOCOL: &str = "td-fetch 1";
/// A head line, including its newline, in either direction. A URL or header
/// past this is refused, not truncated.
const MAX_LINE: usize = 8 * 1024;
/// Header lines a request may carry, and a reply.
const MAX_HEADERS: usize = 64;
/// The most a client may send as a body: a mail with attachments, not an
/// upload service.
const MAX_REQUEST_BODY: u64 = 32 * 1024 * 1024;
/// The most the service reads back for a client that asked for no less; a
/// feed, a mailbox page or an attachment, not an archive.
const MAX_RESPONSE_BODY: u64 = 64 * 1024 * 1024;
/// Redirects followed for GET, at most; a client may ask for as many or
/// fewer with `redirects N`, to follow them itself, and more is refused
/// whatever the method. POST follows none: a redirected body is a
/// different request.
const MAX_REDIRECTS: u32 = 5;
/// Requests served at once; the rest wait in the listen queue. Each holds
/// one body in memory at a time, the request's until the origin has it and
/// then the response's, so this bounds memory at four times the larger
/// body cap as well as sockets.
const WORKERS: usize = 4;
/// The client's whole head and body must arrive within this, and its whole
/// reply must be taken within this again: a budget for the connection, not
/// a per-read timeout that a byte a minute would keep resetting.
const CLIENT_IO_TIMEOUT: Duration = Duration::from_secs(60);
/// The whole exchange with the origin, connect, redirects and a trickling
/// body included: ureq's one deadline, which supersedes its per-read and
/// per-write timeouts, so those are not set. A worker is held by an origin
/// no longer than this, plus `http.rs`'s connect deadline, which ureq
/// applies on its own, and a name lookup, which no deadline reaches.
const ORIGIN_TIMEOUT: Duration = Duration::from_secs(300);
/// The headers that belong to the connection rather than the message
/// (RFC 7230 §6.1) and the framing the service owns. A client naming one is
/// malformed; an origin sending one is not relayed.
const CONNECTION_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];
const SERVICE_HEADERS: &[&str] = &["host", "content-length", "accept-encoding"];
/// What the probe asks for: a loopback URL the policy refuses, so the reply
/// proves the socket, the framing and the policy without a route out.
const PROBE_URL: &str = "http://127.0.0.1:1/";
const PROBE_EXPECTED: &str = "error refused: loopback address";

pub fn run(args: &[String]) {
    let code = match args.get(1).map(String::as_str) {
        Some("run") => match parse_run_args(args.get(2..).unwrap_or(&[])) {
            Ok((socket, policy)) => match serve(Path::new(&socket), policy) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("td-fetchd: {e}");
                    1
                }
            },
            Err(e) => {
                eprintln!("td-fetchd: {e}");
                2
            }
        },
        Some("probe") => match args.get(2) {
            Some(socket) if args.len() == 3 => match probe(Path::new(socket)) {
                Ok(()) => {
                    println!("td-fetchd: ok");
                    0
                }
                Err(e) => {
                    eprintln!("td-fetchd: probe {socket}: {e}");
                    1
                }
            },
            _ => {
                eprintln!("usage: td-fetchd probe SOCKET");
                2
            }
        },
        _ => {
            eprintln!(
                "usage: td-fetchd run --socket PATH [--allow-loopback]\n       td-fetchd probe PATH"
            );
            2
        }
    };
    std::process::exit(code);
}

fn parse_run_args(args: &[String]) -> Result<(String, Policy), String> {
    let mut socket = None;
    let mut policy = Policy {
        allow_loopback: false,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--socket" => {
                socket = Some(
                    it.next()
                        .ok_or_else(|| "--socket needs a path".to_string())?
                        .clone(),
                );
            }
            // For the recipe check and the unit tests, which serve on loopback:
            // never set by a unit.
            "--allow-loopback" => policy.allow_loopback = true,
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok((socket.ok_or_else(|| "--socket is required".to_string())?, policy))
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Policy {
    pub(crate) allow_loopback: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Method {
    Get,
    Post,
}

impl Method {
    fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
        }
    }
}

#[derive(Debug)]
struct Request {
    method: Method,
    url: String,
    headers: Vec<(String, String)>,
    limit: u64,
    /// Redirects the client will have followed for it; the service's
    /// ceiling when absent.
    redirects: Option<u32>,
    body: Vec<u8>,
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// Why a request was not served. `Refused` is policy and `Malformed` is the
/// client's framing; both are answered on the socket. `Transport` is the
/// network's answer, also reported, so the application can say "could not
/// reach" rather than "nothing came back".
#[derive(Debug)]
enum Fault {
    Malformed(String),
    Refused(String),
    Transport(String),
    /// The client closed before its first byte: a liveness probe (td-jail
    /// proves a listener before it creates namespaces; a unit's readiness
    /// check does the same), answered with nothing rather than an error
    /// written into a closed socket.
    Closed,
}

impl Fault {
    fn line(&self) -> String {
        match self {
            Fault::Malformed(m) => format!("error malformed: {m}"),
            Fault::Refused(m) => format!("error refused: {m}"),
            Fault::Transport(m) => format!("error transport: {m}"),
            Fault::Closed => String::new(),
        }
    }
}

fn serve(socket: &Path, policy: Policy) -> Result<(), String> {
    let listener = bind(socket)?;
    let active = Arc::new((Mutex::new(0usize), Condvar::new()));
    loop {
        let (stream, _) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(e) => {
                eprintln!("td-fetchd: accept: {e}");
                continue;
            }
        };
        let slot = Slot::take(&active)?;
        let spawned = std::thread::Builder::new().spawn(move || {
            let _slot = slot;
            handle(stream, policy, CLIENT_IO_TIMEOUT);
        });
        // The stream and the slot went with the closure; when the spawn
        // fails, std drops the closure, the slot with it, and the service
        // goes on.
        if let Err(e) = spawned {
            eprintln!("td-fetchd: spawn worker: {e}");
        }
    }
}

/// One of the `WORKERS` places, given back when dropped, however the worker
/// ends: a place leaked on an unwinding thread would be a place lost for the
/// service's life, and four of those would be the service.
struct Slot(Arc<(Mutex<usize>, Condvar)>);

impl Slot {
    fn take(active: &Arc<(Mutex<usize>, Condvar)>) -> Result<Slot, String> {
        let (count, wake) = &**active;
        let mut count = count.lock().map_err(|_| "worker count poisoned")?;
        while *count >= WORKERS {
            count = wake.wait(count).map_err(|_| "worker count poisoned")?;
        }
        *count += 1;
        Ok(Slot(Arc::clone(active)))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let (count, wake) = &*self.0;
        if let Ok(mut count) = count.lock() {
            *count = count.saturating_sub(1);
        }
        wake.notify_one();
    }
}

/// Bind the socket at `path` with mode 0600, replacing a socket file a
/// previous service left behind (a unit restart), and nothing else; its
/// directory is made, private, when it is absent.
///
/// The listener is bound under a sibling name, given its mode there and
/// renamed over `path`, so the socket is never reachable at its name with
/// the wider mode the umask gives a fresh inode, and a stale file is
/// replaced in one step rather than unlinked and then bound. Two instances
/// starting at once would both find the name unserved and the later rename
/// would take it silently: one instance is the unit's invariant, not this
/// function's.
fn bind(path: &Path) -> Result<UnixListener, String> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if !meta.file_type().is_socket() {
            return Err(format!("{} exists and is not a socket", path.display()));
        }
        if UnixStream::connect(path).is_ok() {
            return Err(format!("{} is already served", path.display()));
        }
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| format!("{} has no directory", path.display()))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("{} has no name", path.display()))?;
    // The socket's directory is the service's own: td-jail binds the
    // directory, so a restart replaces the socket under it without the jail
    // losing it. Made here when absent, with its mode in the one call so it
    // is never seen wider, and never a link.
    match std::fs::symlink_metadata(parent) {
        Ok(meta) if meta.file_type().is_dir() => {}
        Ok(_) => {
            return Err(format!(
                "{} exists and is not a directory",
                parent.display()
            ))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        Err(e) => return Err(format!("{}: {e}", parent.display())),
    }
    let staging = parent.join(format!(".{name}.{}", std::process::id()));
    let _ = std::fs::remove_file(&staging);
    let listener = UnixListener::bind(&staging)
        .map_err(|e| format!("bind {}: {e}", staging.display()))?;
    std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("chmod 0600 {}: {e}", staging.display()))?;
    std::fs::rename(&staging, path).map_err(|e| {
        let _ = std::fs::remove_file(&staging);
        format!("rename {} to {}: {e}", staging.display(), path.display())
    })?;
    Ok(listener)
}

/// A stream with a budget: every read or write first sets the socket's
/// timeout to what is left, so the client's head and body, and then its
/// reply, each complete within `budget` in all rather than per byte.
struct Bounded {
    stream: UnixStream,
    deadline: Instant,
}

impl Bounded {
    fn new(stream: UnixStream, budget: Duration) -> Bounded {
        let deadline = Instant::now()
            .checked_add(budget)
            .unwrap_or_else(Instant::now);
        Bounded { stream, deadline }
    }

    fn remaining(&self) -> io::Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or_else(time_is_up)
    }
}

fn time_is_up() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "the connection's time is up")
}

/// The socket's own timeout, set to what was left, reports as `WouldBlock`;
/// it is the same deadline.
fn at_the_deadline(error: io::Error) -> io::Error {
    if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) {
        time_is_up()
    } else {
        error
    }
}

impl Read for Bounded {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.remaining()?;
        self.stream.set_read_timeout(Some(left))?;
        self.stream.read(buf).map_err(at_the_deadline)
    }
}

impl Write for Bounded {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let left = self.remaining()?;
        self.stream.set_write_timeout(Some(left))?;
        self.stream.write(buf).map_err(at_the_deadline)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

fn handle(stream: UnixStream, policy: Policy, budget: Duration) {
    let Ok(writer) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(Bounded::new(stream, budget));
    let outcome = read_request(&mut reader).and_then(|request| perform(request, policy));
    let mut writer = Bounded::new(writer, budget);
    let written = match outcome {
        Ok(reply) => write_reply(&mut writer, &reply),
        Err(Fault::Closed) => Ok(()),
        Err(fault) => write_fault(&mut writer, &fault),
    };
    if let Err(e) = written {
        eprintln!("td-fetchd: reply: {e}");
    }
}

fn read_line(reader: &mut impl BufRead) -> Result<Option<String>, Fault> {
    let mut raw = Vec::with_capacity(128);
    let read = reader
        .by_ref()
        .take(MAX_LINE as u64)
        .read_until(b'\n', &mut raw)
        .map_err(|e| Fault::Malformed(format!("read: {e}")))?;
    if read == 0 {
        return Ok(None);
    }
    if raw.last() != Some(&b'\n') {
        return Err(Fault::Malformed(format!(
            "a line past {MAX_LINE} bytes, or unterminated"
        )));
    }
    raw.pop();
    if raw.iter().any(|byte| byte.is_ascii_control()) {
        return Err(Fault::Malformed("a control byte in a head line".into()));
    }
    String::from_utf8(raw)
        .map(Some)
        .map_err(|_| Fault::Malformed("a head line that is not UTF-8".into()))
}

fn read_request(reader: &mut impl BufRead) -> Result<Request, Fault> {
    match read_line(reader)? {
        Some(line) if line == PROTOCOL => {}
        None => return Err(Fault::Closed),
        Some(_) => return Err(Fault::Malformed(format!("first line is not {PROTOCOL:?}"))),
    }
    let mut method = None;
    let mut url = None;
    let mut headers = Vec::new();
    let mut limit = MAX_RESPONSE_BODY;
    let mut redirects = None;
    let mut body_len = 0u64;
    let mut seen = std::collections::BTreeSet::new();
    loop {
        let line = read_line(reader)?
            .ok_or_else(|| Fault::Malformed("head ended without a blank line".into()))?;
        if line.is_empty() {
            break;
        }
        let (key, value) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        if key != "header" && !seen.insert(key.to_string()) {
            return Err(Fault::Malformed(format!("key {key:?} repeated")));
        }
        match key {
            "method" => {
                method = Some(match value {
                    "GET" => Method::Get,
                    "POST" => Method::Post,
                    other => return Err(Fault::Malformed(format!("method {other:?}"))),
                })
            }
            "url" => {
                if value.is_empty() {
                    return Err(Fault::Malformed("url is empty".into()));
                }
                url = Some(value.to_string());
            }
            "header" => {
                if headers.len() >= MAX_HEADERS {
                    return Err(Fault::Malformed(format!("more than {MAX_HEADERS} headers")));
                }
                headers.push(parse_header(value)?);
            }
            "limit" => {
                limit = value
                    .parse::<u64>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| Fault::Malformed(format!("limit {value:?}")))?
                    .min(MAX_RESPONSE_BODY);
            }
            "body" => {
                body_len = value
                    .parse::<u64>()
                    .map_err(|_| Fault::Malformed(format!("body {value:?}")))?;
            }
            "redirects" => {
                let count = value
                    .parse::<u32>()
                    .map_err(|_| Fault::Malformed(format!("redirects {value:?}")))?;
                if count > MAX_REDIRECTS {
                    return Err(Fault::Refused(format!("redirects over {MAX_REDIRECTS}")));
                }
                redirects = Some(count);
            }
            other => return Err(Fault::Malformed(format!("head key {other:?}"))),
        }
    }
    let method = method.ok_or_else(|| Fault::Malformed("no method".into()))?;
    let url = url.ok_or_else(|| Fault::Malformed("no url".into()))?;
    if body_len > MAX_REQUEST_BODY {
        return Err(Fault::Refused(format!(
            "request body over {MAX_REQUEST_BODY} bytes"
        )));
    }
    if method == Method::Get && body_len > 0 {
        return Err(Fault::Malformed("a GET with a body".into()));
    }
    if method == Method::Post && redirects.unwrap_or(0) > 0 {
        return Err(Fault::Malformed("a POST with redirects".into()));
    }
    let mut body = vec![0u8; body_len as usize];
    reader
        .read_exact(&mut body)
        .map_err(|e| Fault::Malformed(format!("body short: {e}")))?;
    Ok(Request {
        method,
        url,
        headers,
        limit,
        redirects,
        body,
    })
}

/// `name: value`, the name a lower-case token. The connection's headers and
/// the framing are the service's to set, so a client naming one is malformed
/// rather than silently overridden. `user-agent` is the client's to set,
/// over the service's default.
fn parse_header(text: &str) -> Result<(String, String), Fault> {
    let (name, value) = text
        .split_once(':')
        .ok_or_else(|| Fault::Malformed(format!("header {text:?} has no colon")))?;
    let value = value.strip_prefix(' ').unwrap_or(value);
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(Fault::Malformed(format!(
            "header name {name:?} is not a lower-case token"
        )));
    }
    if CONNECTION_HEADERS.contains(&name) || SERVICE_HEADERS.contains(&name) {
        return Err(Fault::Malformed(format!("header {name:?} is the service's")));
    }
    Ok((name.to_string(), value.to_string()))
}

/// The resolver ureq calls before every connection it makes, the redirected
/// ones included, handed the host and port ureq itself parsed. The address
/// policy lives here and nowhere else: a URL the service parsed separately
/// could be read differently by ureq's parser (a backslash in the authority
/// is one such reading), and a redirect never passes through the request
/// URL at all. A refusal is recorded so the reply can name it exactly
/// rather than as the transport error ureq wraps it in.
struct PolicyResolver {
    policy: Policy,
    refusal: Mutex<Option<String>>,
}

impl ureq::Resolver for PolicyResolver {
    fn resolve(&self, netloc: &str) -> io::Result<Vec<SocketAddr>> {
        let addresses: Vec<SocketAddr> = netloc.to_socket_addrs()?.collect();
        if addresses.is_empty() {
            return Err(io::Error::other(format!("{netloc}: no address")));
        }
        for address in &addresses {
            if let Some(reason) = refused_address(address.ip(), self.policy) {
                if let Ok(mut refusal) = self.refusal.lock() {
                    *refusal = Some(reason.to_string());
                }
                return Err(io::Error::other(format!("{netloc}: {reason}")));
            }
        }
        Ok(addresses)
    }
}

/// ureq takes its resolver by value; this hands it the shared one so the
/// request can read back the refusal afterwards.
struct SharedResolver(Arc<PolicyResolver>);

impl ureq::Resolver for SharedResolver {
    fn resolve(&self, netloc: &str) -> io::Result<Vec<SocketAddr>> {
        self.0.resolve(netloc)
    }
}

fn perform(request: Request, policy: Policy) -> Result<Reply, Fault> {
    check_scheme(&request.url)?;
    let resolver = Arc::new(PolicyResolver {
        policy,
        refusal: Mutex::new(None),
    });
    // One deadline for the exchange: ureq applies it in place of its
    // per-read and per-write timeouts, so setting those would do nothing.
    // The connect deadline is applied on its own.
    let agent = ureq::AgentBuilder::new()
        .resolver(SharedResolver(Arc::clone(&resolver)))
        .timeout(ORIGIN_TIMEOUT)
        .timeout_connect(crate::http::CONNECT_TIMEOUT)
        .redirects(ureq_redirects(match request.method {
            Method::Get => request.redirects.unwrap_or(MAX_REDIRECTS),
            Method::Post => 0,
        }))
        .user_agent("td-fetchd/1")
        .build();
    let Request {
        method,
        url,
        headers,
        limit,
        redirects: _,
        body: request_body,
    } = request;
    let mut call = agent.request(method.as_str(), &url);
    for (name, value) in &headers {
        call = call.set(name, value);
    }
    call = call.set("accept-encoding", "identity");
    let response = match method {
        Method::Get => call.call(),
        Method::Post => call.send_bytes(&request_body),
    };
    // The origin has the request body; a worker holds it or the response's,
    // never both.
    drop(request_body);
    let response = match response {
        Ok(response) => response,
        // An answer is an answer: the application reads the status.
        Err(ureq::Error::Status(_, response)) => response,
        Err(ureq::Error::Transport(transport)) => {
            let refusal = resolver.refusal.lock().ok().and_then(|refusal| refusal.clone());
            return Err(match refusal {
                Some(reason) => Fault::Refused(reason),
                None => Fault::Transport(transport.to_string()),
            });
        }
    };
    let status = response.status();
    // Each distinct name once, its values in order: `headers_names` repeats
    // a name that appears twice, and asking for all of its values twice
    // would square them.
    let mut names: Vec<String> = Vec::new();
    for name in response.headers_names() {
        let lower = name.to_ascii_lowercase();
        if !names.contains(&lower) {
            names.push(lower);
        }
    }
    let mut headers = Vec::new();
    for name in names {
        if CONNECTION_HEADERS.contains(&name.as_str()) {
            continue;
        }
        for value in response.all(&name) {
            if headers.len() >= MAX_HEADERS {
                return Err(Fault::Refused(format!("response over {MAX_HEADERS} headers")));
            }
            // The reply keeps the request's line bound: `header name: value\n`.
            if "header ".len() + name.len() + ": ".len() + value.len() + 1 > MAX_LINE {
                return Err(Fault::Refused(format!(
                    "response header over {MAX_LINE} bytes"
                )));
            }
            headers.push((name.clone(), value.to_string()));
        }
    }
    let mut body = Vec::new();
    response
        .into_reader()
        .take(limit.saturating_add(1))
        .read_to_end(&mut body)
        .map_err(|e| Fault::Transport(format!("read body: {e}")))?;
    if body.len() as u64 > limit {
        return Err(Fault::Refused(format!("response over {limit} bytes")));
    }
    Ok(Reply {
        status,
        headers,
        body,
    })
}

/// The scheme is the one part of the URL judged here, and it is judged on
/// the text before `://`, which no parser reads differently, in either case
/// as the url crate reads it; a redirect to another scheme fails inside
/// ureq, which speaks only these two.
fn check_scheme(url: &str) -> Result<(), Fault> {
    let (scheme, _) = url
        .split_once("://")
        .ok_or_else(|| Fault::Malformed("url has no scheme".into()))?;
    if !(scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")) {
        return Err(Fault::Refused(format!("scheme {scheme:?}")));
    }
    Ok(())
}

/// The addresses the grant must not reach: td's own listeners and the
/// link, in either family, the v4-mapped and the deprecated v4-compatible
/// forms included.
fn refused_address(ip: IpAddr, policy: Policy) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => refused_v4(v4, policy),
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                return (!policy.allow_loopback).then_some("loopback address");
            }
            if v6.is_unspecified() {
                return Some("unspecified address");
            }
            if v6.is_unicast_link_local() {
                return Some("link-local address");
            }
            if v6.is_multicast() {
                return Some("broadcast or multicast address");
            }
            v6.to_ipv4().and_then(|v4| refused_v4(v4, policy))
        }
    }
}

fn refused_v4(v4: Ipv4Addr, policy: Policy) -> Option<&'static str> {
    if v4.is_loopback() {
        return (!policy.allow_loopback).then_some("loopback address");
    }
    // All of 0.0.0.0/8, not only 0.0.0.0: Linux delivers a connect to any
    // address in it to this host.
    let [first, ..] = v4.octets();
    if first == 0 {
        return Some("unspecified address");
    }
    if v4.is_link_local() {
        return Some("link-local address");
    }
    if v4.is_broadcast() || v4.is_multicast() {
        return Some("broadcast or multicast address");
    }
    None
}

fn write_reply(writer: &mut impl Write, reply: &Reply) -> io::Result<()> {
    let mut head = format!("{PROTOCOL}\nstatus {}\n", reply.status);
    for (name, value) in &reply.headers {
        head.push_str("header ");
        head.push_str(name);
        head.push_str(": ");
        // A value with a newline would end the head early; the client sees
        // spaces instead, and the bytes it cares about are the body's.
        head.extend(value.chars().map(|c| if c.is_ascii_control() { ' ' } else { c }));
        head.push('\n');
    }
    head.push_str(&format!("body {}\n\n", reply.body.len()));
    writer.write_all(head.as_bytes())?;
    writer.write_all(&reply.body)?;
    writer.flush()
}

fn write_fault(writer: &mut impl Write, fault: &Fault) -> io::Result<()> {
    let line = bounded_line(&fault.line());
    writer.write_all(format!("{PROTOCOL}\n{line}\n\n").as_bytes())?;
    writer.flush()
}

/// The fault line keeps the head's bound. A transport error carries the
/// client's URL, which may be most of a legal line by itself, so the line
/// is cut at the bound rather than refused: a refusal of the refusal would
/// leave the client nothing. A control byte in it would end the line early
/// and is a space.
fn bounded_line(text: &str) -> String {
    let mut line: String = text
        .chars()
        .map(|c| if c.is_ascii_control() { ' ' } else { c })
        .collect();
    // Room for the newline.
    let mut end = MAX_LINE - 1;
    if line.len() > end {
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        line.truncate(end);
    }
    line
}

/// Connect, ask for the loopback URL, expect the policy's refusal: the
/// socket is served, the framing is understood and the policy is on.
fn probe(socket: &Path) -> Result<(), String> {
    let mut stream =
        UnixStream::connect(socket).map_err(|e| format!("connect: {e}"))?;
    stream
        .set_read_timeout(Some(CLIENT_IO_TIMEOUT))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(format!("{PROTOCOL}\nmethod GET\nurl {PROBE_URL}\n\n").as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    let mut reader = BufReader::new(stream);
    let mut lines = Vec::new();
    for _ in 0..2 {
        let line = read_line(&mut reader)
            .map_err(|fault| format!("read: {}", fault.line()))?
            .ok_or_else(|| "the service closed without a reply".to_string())?;
        lines.push(line);
    }
    if lines.first().map(String::as_str) != Some(PROTOCOL) {
        return Err(format!("first line {:?}, not {PROTOCOL:?}", lines.first()));
    }
    if lines.get(1).map(String::as_str) != Some(PROBE_EXPECTED) {
        return Err(format!("second line {:?}, not {PROBE_EXPECTED:?}", lines.get(1)));
    }
    Ok(())
}

/// ureq's `redirects` counts requests, not the redirects followed: given N
/// it follows N - 1 and calls the Nth 3xx one too many, and given 0 it hands
/// the 3xx back. So N followed is N + 1 asked of it, and none is none.
fn ureq_redirects(followed: u32) -> u32 {
    if followed == 0 {
        0
    } else {
        followed.saturating_add(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use ureq::Resolver as _;

    #[test]
    fn redirects_asked_for_are_redirects_followed() {
        assert_eq!(ureq_redirects(0), 0);
        assert_eq!(ureq_redirects(1), 2);
        assert_eq!(ureq_redirects(MAX_REDIRECTS), MAX_REDIRECTS + 1);
    }

    const STRICT: Policy = Policy {
        allow_loopback: false,
    };
    const LENIENT: Policy = Policy {
        allow_loopback: true,
    };
    /// Enough for a local exchange, short enough for the slow-client test.
    const TEST_BUDGET: Duration = Duration::from_secs(2);

    fn request_bytes(head: &str, body: &[u8]) -> Vec<u8> {
        let mut bytes = format!("{PROTOCOL}\n{head}\n").into_bytes();
        bytes.extend_from_slice(body);
        bytes
    }

    fn parse(bytes: &[u8]) -> Result<Request, String> {
        read_request(&mut BufReader::new(bytes)).map_err(|f| f.line())
    }

    #[test]
    fn a_head_is_read_as_written_and_a_bad_one_is_named() {
        let bytes = request_bytes(
            "method POST\nurl https://h/p\nheader accept: text/xml\nlimit 10\nbody 3\n",
            b"abc",
        );
        let request = parse(&bytes).unwrap();
        assert_eq!(request.method, Method::Post);
        assert_eq!(request.url, "https://h/p");
        assert_eq!(request.headers, [("accept".to_string(), "text/xml".to_string())]);
        assert_eq!(request.limit, 10);
        assert_eq!(request.body, b"abc");
        // Defaults: no body, the service's own limit.
        let request = parse(&request_bytes("method GET\nurl http://h/\n", b"")).unwrap();
        assert_eq!((request.body.len(), request.limit), (0, MAX_RESPONSE_BODY));
        for (head, reason) in [
            ("method PUT\nurl http://h/\n", "method"),
            ("url http://h/\n", "no method"),
            ("method GET\n", "no url"),
            ("method GET\nurl http://h/\nheader Accept: x\n", "lower-case token"),
            ("method GET\nurl http://h/\nheader host: h\n", "the service's"),
            ("method GET\nurl http://h/\nheader upgrade: h2c\n", "the service's"),
            ("method GET\nurl http://h/\nheader te: trailers\n", "the service's"),
            ("method GET\nurl \n", "url is empty"),
            ("method GET\nurl http://h/\nbody 1\n", "a GET with a body"),
            ("method GET\nurl http://h/\nlimit 0\n", "limit"),
            ("method GET\nurl http://h/\nextra 1\n", "head key"),
            ("method GET\nmethod GET\nurl http://h/\n", "repeated"),
            ("method GET\nurl http://h/\nurl http://h/\n", "repeated"),
            ("method POST\nurl http://h/\nbody 1\nbody 1\n", "repeated"),
        ] {
            let err = parse(&request_bytes(head, b"x")).unwrap_err();
            assert!(err.contains(reason), "{head:?}: {err}");
        }
        let err = parse(b"td-fetch 2\n\n").unwrap_err();
        assert!(err.contains("first line"), "{err}");
        assert!(matches!(read_request(&mut BufReader::new(&b""[..])), Err(Fault::Closed)));
        let err = parse(&request_bytes("method GET\nurl http://h/\r\n", b"")).unwrap_err();
        assert!(err.contains("control byte"), "{err}");
        let long = format!("method GET\nurl http://h/{}\n", "a".repeat(MAX_LINE));
        let err = parse(&request_bytes(&long, b"")).unwrap_err();
        assert!(err.contains("past"), "{err}");
        let many = format!(
            "method GET\nurl http://h/\n{}",
            "header x-n: v\n".repeat(MAX_HEADERS + 1)
        );
        let err = parse(&request_bytes(&many, b"")).unwrap_err();
        assert!(err.contains(&format!("more than {MAX_HEADERS} headers")), "{err}");
        let enough = format!(
            "method GET\nurl http://h/\n{}",
            "header x-n: v\n".repeat(MAX_HEADERS)
        );
        assert_eq!(parse(&request_bytes(&enough, b"")).unwrap().headers.len(), MAX_HEADERS);
        let err = parse(&request_bytes("method POST\nurl http://h/\nbody 5\n", b"ab")).unwrap_err();
        assert!(err.contains("body short"), "{err}");
        let err = parse(&request_bytes(
            &format!("method POST\nurl http://h/\nbody {}\n", MAX_REQUEST_BODY + 1),
            b"",
        ))
        .unwrap_err();
        assert!(err.contains("request body over"), "{err}");
    }

    #[test]
    fn the_policy_refuses_what_the_grant_must_not_reach() {
        for (url, reason) in [
            ("file:///etc/passwd", "scheme \"file\""),
            ("ftp://h/", "scheme \"ftp\""),
            ("FILE://h/", "scheme \"FILE\""),
        ] {
            let err = check_scheme(url).err().map(|f| f.line()).unwrap_or_default();
            assert!(err.contains(reason), "{url}: {err:?}");
        }
        assert!(check_scheme("h/p").unwrap_err().line().contains("no scheme"));
        assert!(check_scheme("https://h/p").is_ok());
        assert!(check_scheme("HTTP://h/p").is_ok());
        // The run arguments: the socket is required, the flag is off unless given.
        let (socket, policy) = parse_run_args(&["--socket".into(), "/s".into()]).unwrap();
        assert_eq!((socket.as_str(), policy.allow_loopback), ("/s", false));
        let (_, policy) =
            parse_run_args(&["--socket".into(), "/s".into(), "--allow-loopback".into()]).unwrap();
        assert!(policy.allow_loopback);
        assert!(parse_run_args(&["--allow-loopback".into()]).unwrap_err().contains("required"));
        assert!(parse_run_args(&["--socket".into()]).unwrap_err().contains("needs a path"));
        assert!(parse_run_args(&["--socket".into(), "/s".into(), "-v".into()]).unwrap_err().contains("unknown"));
        for (address, reason) in [
            ("127.0.0.1", "loopback address"),
            ("127.5.6.7", "loopback address"),
            ("::1", "loopback address"),
            ("::ffff:127.0.0.1", "loopback address"),
            ("::127.0.0.1", "loopback address"),
            ("0.0.0.0", "unspecified address"),
            ("0.0.0.1", "unspecified address"),
            ("::ffff:0.0.0.1", "unspecified address"),
            ("::", "unspecified address"),
            ("169.254.169.254", "link-local address"),
            ("fe80::1", "link-local address"),
            ("255.255.255.255", "broadcast or multicast address"),
            ("224.0.0.1", "broadcast or multicast address"),
            ("ff02::1", "broadcast or multicast address"),
        ] {
            let ip: IpAddr = address.parse().unwrap();
            assert_eq!(refused_address(ip, STRICT), Some(reason), "{address}");
        }
        for address in ["93.184.216.34", "2606:2800:21f:cb07:6820:80da:af6b:8b2c", "10.0.0.1"] {
            let ip: IpAddr = address.parse().unwrap();
            assert_eq!(refused_address(ip, STRICT), None, "{address}");
        }
        // The test flag admits loopback and nothing else.
        assert_eq!(refused_address("127.0.0.1".parse().unwrap(), LENIENT), None);
        assert_eq!(refused_address("::1".parse().unwrap(), LENIENT), None);
        assert_eq!(
            refused_address("0.0.0.1".parse().unwrap(), LENIENT),
            Some("unspecified address")
        );
        // The resolver applies it to what ureq hands over, records the
        // refusal, and resolves a literal without a resolver.
        let resolver = PolicyResolver {
            policy: STRICT,
            refusal: Mutex::new(None),
        };
        let resolved = resolver.resolve("93.184.216.34:443").unwrap();
        assert_eq!(resolved, ["93.184.216.34:443".parse::<SocketAddr>().unwrap()]);
        assert_eq!(*resolver.refusal.lock().unwrap(), None);
        for (netloc, reason) in [
            ("127.0.0.1:8080", "loopback address"),
            ("[::1]:80", "loopback address"),
            ("[::ffff:127.0.0.1]:80", "loopback address"),
            ("0.0.0.1:80", "unspecified address"),
            ("169.254.169.254:80", "link-local address"),
        ] {
            let err = resolver.resolve(netloc).unwrap_err().to_string();
            assert!(err.contains(reason), "{netloc}: {err}");
            assert_eq!(resolver.refusal.lock().unwrap().as_deref(), Some(reason), "{netloc}");
        }
    }

    #[test]
    fn a_slot_is_given_back_when_its_worker_ends() {
        let active = Arc::new((Mutex::new(0usize), Condvar::new()));
        let slot = Slot::take(&active).unwrap();
        assert_eq!(*active.0.lock().unwrap(), 1);
        drop(slot);
        assert_eq!(*active.0.lock().unwrap(), 0);
        // However it ends: a worker that unwinds gives its place back too,
        // and the count is not poisoned by it.
        let held = Arc::clone(&active);
        let worker = std::thread::spawn(move || {
            let _slot = Slot::take(&held).unwrap();
            panic!("a worker unwinding");
        });
        assert!(worker.join().is_err());
        assert_eq!(*active.0.lock().unwrap(), 0);
        assert!(Slot::take(&active).is_ok());
    }

    /// A canned HTTP/1.1 origin on loopback: one request per accept, the
    /// response chosen by path, and the request's head and body kept for
    /// the test to look at. It stops after `connections` or after
    /// `ORIGIN_PATIENCE`, whichever is first, so a count that no longer
    /// matches ureq's behaviour is a failed assertion on what it saw, not a
    /// test that never ends.
    const ORIGIN_PATIENCE: Duration = Duration::from_secs(20);

    fn origin(
        connections: usize,
        responses: Vec<(&'static str, String)>,
    ) -> Option<(u16, std::thread::JoinHandle<Vec<Vec<u8>>>)> {
        let listener = TcpListener::bind("127.0.0.1:0").ok()?;
        let port = listener.local_addr().ok()?.port();
        listener.set_nonblocking(true).ok()?;
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            let deadline = Instant::now() + ORIGIN_PATIENCE;
            while seen.len() < connections {
                if Instant::now() > deadline {
                    break;
                }
                let mut conn = match listener.accept() {
                    Ok((conn, _)) => conn,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(_) => break,
                };
                let _ = conn.set_nonblocking(false);
                let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = conn.set_write_timeout(Some(Duration::from_secs(5)));
                let raw = read_http_request(&mut conn);
                let path = raw
                    .split(|b| *b == b' ')
                    .nth(1)
                    .map(|p| String::from_utf8_lossy(p).into_owned())
                    .unwrap_or_default();
                let body = responses
                    .iter()
                    .find(|(p, _)| *p == path)
                    .map(|(_, r)| r.clone())
                    .unwrap_or_else(|| "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into());
                let _ = conn.write_all(body.as_bytes());
                let _ = conn.flush();
                seen.push(raw);
            }
            seen
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

    fn response(status: &str, headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
            body.len()
        )
    }

    fn redirect(to: &str) -> String {
        format!("HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    }

    fn served(socket: &Path, policy: Policy) -> Option<()> {
        let listener = bind(socket).ok()?;
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                handle(stream, policy, TEST_BUDGET);
            }
        });
        Some(())
    }

    fn ask(socket: &Path, head: &str, body: &[u8]) -> (Vec<String>, Vec<u8>) {
        let mut stream = UnixStream::connect(socket).unwrap();
        stream.write_all(&request_bytes(head, body)).unwrap();
        read_reply(stream)
    }

    fn read_reply(stream: UnixStream) -> (Vec<String>, Vec<u8>) {
        let mut reader = BufReader::new(stream);
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let line = line.trim_end_matches('\n').to_string();
            if line.is_empty() {
                break;
            }
            lines.push(line);
        }
        let mut body = Vec::new();
        reader.read_to_end(&mut body).unwrap();
        (lines, body)
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("td-fetchd-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_request_is_served_over_the_socket_within_its_bounds() {
        let dir = scratch("serve");
        let socket = dir.join("td-fetch");
        let Some(()) = served(&socket, LENIENT) else { return };
        assert_eq!(
            std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // The staging name is gone: the directory holds the socket alone.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        // Fourteen connections to the origin: a followed redirect costs two,
        // asked for or not, one handed back to the client one, one hop too
        // many two, and the redirect inward is refused before its second hop.
        let Some((port, origin)) = origin(14, vec![
            ("/feed", response("200 OK", "Content-Type: text/xml\r\nX-Two: a\r\nX-Two: b\r\nX-Ctl: a\tb\r\nKeep-Alive: timeout=5\r\n", "<rss/>")),
            ("/post", response("201 Created", "", "made")),
            ("/big", response("200 OK", "", &"z".repeat(100))),
            ("/moved", redirect("/feed")),
            ("/twice", redirect("/moved")),
            ("/postmoved", redirect("/feed")),
            ("/teapot", response("418 I'm a teapot", "", "short and stout")),
            ("/inward", redirect("http://0.0.0.1:1/")),
            ("/wide", response("200 OK", &format!("X-Wide: {}\r\n", "w".repeat(MAX_LINE)), "")),
        ]) else {
            return;
        };
        let base = format!("http://127.0.0.1:{port}");
        // GET: status, headers in order and lower case, each value once, the body.
        let (lines, body) = ask(
            &socket,
            &format!("method GET\nurl {base}/feed\nheader accept: text/xml\n"),
            b"",
        );
        assert_eq!(lines[0], PROTOCOL);
        assert_eq!(lines[1], "status 200");
        assert!(lines.contains(&"header content-type: text/xml".to_string()), "{lines:?}");
        assert_eq!(lines.iter().filter(|l| *l == "header x-two: a").count(), 1, "{lines:?}");
        assert_eq!(lines.iter().filter(|l| *l == "header x-two: b").count(), 1, "{lines:?}");
        // A control byte in a value is a space; the connection's headers are not relayed.
        assert!(lines.contains(&"header x-ctl: a b".to_string()), "{lines:?}");
        assert!(!lines.iter().any(|l| l.starts_with("header connection")), "{lines:?}");
        assert!(!lines.iter().any(|l| l.starts_with("header keep-alive")), "{lines:?}");
        assert_eq!(lines.last().map(String::as_str), Some("body 6"));
        assert_eq!(body, b"<rss/>");
        // POST: the body and the headers reach the origin, identity encoding asked.
        let (lines, body) = ask(
            &socket,
            &format!("method POST\nurl {base}/post\nheader content-type: text/plain\nbody 5\n"),
            b"hello",
        );
        assert_eq!(lines[1], "status 201");
        assert_eq!(body, b"made");
        // Over the client's limit: refused, with the limit named.
        let (lines, body) = ask(&socket, &format!("method GET\nurl {base}/big\nlimit 99\n"), b"");
        assert_eq!(lines, [PROTOCOL, "error refused: response over 99 bytes"]);
        assert!(body.is_empty());
        // A redirect is followed for GET and not for POST.
        let (lines, body) = ask(&socket, &format!("method GET\nurl {base}/moved\n"), b"");
        assert_eq!(lines[1], "status 200");
        assert_eq!(body, b"<rss/>");
        let (lines, _) = ask(&socket, &format!("method POST\nurl {base}/postmoved\nbody 0\n"), b"");
        assert_eq!(lines[1], "status 302");
        // A client that follows redirects itself, to carry a credential the
        // service would drop, asks for none and is handed the redirect; more
        // than the ceiling is refused whatever the method, and a POST asking
        // for one to five is malformed.
        let (lines, _) = ask(&socket, &format!("method GET\nurl {base}/moved\nredirects 0\n"), b"");
        assert_eq!(lines[1], "status 302");
        assert!(lines.iter().any(|line| line == "header location: /feed"), "{lines:?}");
        let (lines, _) = ask(&socket, &format!("method GET\nurl {base}/moved\nredirects 6\n"), b"");
        assert_eq!(lines[1], format!("error refused: redirects over {MAX_REDIRECTS}"));
        let (lines, _) = ask(&socket, &format!("method POST\nurl {base}/post\nredirects 1\nbody 0\n"), b"");
        assert_eq!(lines[1], "error malformed: a POST with redirects");
        let (lines, _) = ask(&socket, &format!("method POST\nurl {base}/post\nredirects 9\nbody 0\n"), b"");
        assert_eq!(lines[1], format!("error refused: redirects over {MAX_REDIRECTS}"));
        // One asked for is one followed, not one request: the hop lands, and
        // a second hop on that budget is ureq's refusal, handed on.
        let (lines, body) = ask(&socket, &format!("method GET\nurl {base}/moved\nredirects 1\n"), b"");
        assert_eq!(lines[1], "status 200");
        assert_eq!(body, b"<rss/>");
        let (lines, _) = ask(&socket, &format!("method GET\nurl {base}/twice\nredirects 1\n"), b"");
        assert!(
            lines[1].starts_with("error transport: ") && lines[1].contains("Too Many Redirects"),
            "{lines:?}"
        );
        // An answer over 400 is an answer.
        let (lines, body) = ask(&socket, &format!("method GET\nurl {base}/teapot\n"), b"");
        assert_eq!(lines[1], "status 418");
        assert_eq!(body, b"short and stout");
        // A redirect to a refused address is refused where it is followed.
        let (lines, _) = ask(&socket, &format!("method GET\nurl {base}/inward\n"), b"");
        assert_eq!(lines[1], "error refused: unspecified address");
        // A response header the reply's line bound cannot carry is refused.
        let (lines, _) = ask(&socket, &format!("method GET\nurl {base}/wide\n"), b"");
        assert_eq!(lines[1], format!("error refused: response header over {MAX_LINE} bytes"));
        // Policy and framing faults are answered on the socket too.
        let (lines, _) = ask(&socket, "method GET\nurl ftp://127.0.0.1/\n", b"");
        assert_eq!(lines[1], "error refused: scheme \"ftp\"");
        let (lines, _) = ask(&socket, "method GET\n", b"");
        assert_eq!(lines[1], "error malformed: no url");
        // The address policy is applied to what ureq parses: this authority
        // reads as a public host to a naive split and as 0.0.0.1 to ureq.
        let (lines, _) = ask(&socket, "method GET\nurl http://0.0.0.1:1\\@93.184.216.34/\n", b"");
        assert_eq!(lines[1], "error refused: unspecified address");
        // A connect-and-close is a liveness probe: nothing is written back
        // and the next request is served.
        drop(UnixStream::connect(&socket).unwrap());
        let (lines, _) = ask(&socket, "method GET\nurl ftp://127.0.0.1/\n", b"");
        assert_eq!(lines[1], "error refused: scheme \"ftp\"");
        // A closed port is the network's answer.
        let (lines, _) = ask(&socket, "method GET\nurl http://127.0.0.1:1/\n", b"");
        assert!(lines[1].starts_with("error transport: "), "{lines:?}");
        // A fault line keeps the bound too: a transport error carries the
        // URL, and a legal URL is most of a line.
        let long = format!("http://127.0.0.1:1/{}", "p".repeat(MAX_LINE - 64));
        let (lines, _) = ask(&socket, &format!("method GET\nurl {long}\n"), b"");
        assert!(lines[1].starts_with("error transport: "), "{}", lines[1]);
        assert!(lines[1].contains("http://127.0.0.1:1/ppp"), "{}", lines[1]);
        assert_eq!(lines[1].len(), MAX_LINE - 1);
        let seen = origin.join().unwrap();
        assert_eq!(seen.len(), 14, "origin connections");
        let first = String::from_utf8_lossy(&seen[0]).to_ascii_lowercase();
        assert!(first.starts_with("get /feed http/1.1"), "{first}");
        assert!(first.contains("accept: text/xml"), "{first}");
        assert!(first.contains("accept-encoding: identity"), "{first}");
        assert!(first.contains("user-agent: td-fetchd/1"), "{first}");
        let second = String::from_utf8_lossy(&seen[1]).to_ascii_lowercase();
        assert!(second.starts_with("post /post http/1.1"), "{second}");
        assert!(second.contains("content-type: text/plain"), "{second}");
        assert!(second.ends_with("hello"), "{second}");
        // The probe, against this lenient service, sees the transport error
        // rather than the refusal it expects: the flag is not for units.
        let err = probe(&socket).unwrap_err();
        assert!(err.contains("second line"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_slow_client_is_cut_off_at_the_budget_and_told() {
        let dir = scratch("slow");
        let socket = dir.join("td-fetch");
        let Some(()) = served(&socket, LENIENT) else { return };
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream.write_all(format!("{PROTOCOL}\nmethod GET\n").as_bytes()).unwrap();
        std::thread::sleep(TEST_BUDGET + Duration::from_millis(500));
        let (lines, _) = read_reply(stream);
        assert_eq!(lines[0], PROTOCOL);
        assert!(lines[1].starts_with("error malformed: read: "), "{lines:?}");
        assert!(lines[1].contains("time is up"), "{lines:?}");
        // The loop went on: the next request is served.
        let (lines, _) = ask(&socket, "method GET\nurl ftp://127.0.0.1/\n", b"");
        assert_eq!(lines[1], "error refused: scheme \"ftp\"");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_probe_expects_the_policys_own_refusal() {
        let dir = scratch("probe");
        let socket = dir.join("td-fetch");
        let Some(()) = served(&socket, STRICT) else { return };
        probe(&socket).unwrap();
        // A served socket is not rebound; a stale one is replaced in place.
        assert!(bind(&socket).unwrap_err().contains("already served"));
        let stale = dir.join("stale");
        drop(UnixListener::bind(&stale).unwrap());
        let listener = bind(&stale).unwrap();
        assert_eq!(std::fs::metadata(&stale).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(UnixStream::connect(&stale).is_ok());
        drop(listener);
        let file = dir.join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(bind(&file).unwrap_err().contains("not a socket"));
        // The socket's directory is made when absent, private, and holds
        // the socket alone; a directory that is not one is refused.
        let service = dir.join("service");
        let listener = bind(&service.join("socket")).unwrap();
        assert_eq!(
            std::fs::metadata(&service).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(std::fs::read_dir(&service).unwrap().count(), 1);
        drop(listener);
        assert!(bind(&file.join("socket")).unwrap_err().contains("not a directory"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
