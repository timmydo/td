//! A td-fetch socket for the offline tests (APPLICATIONS.md §W.8): the
//! protocol td-agent's client speaks to td-fetchd, served from a thread at
//! `RUNTIME/td-fetch/socket`, answering each request with the next of a
//! script of recorded exchanges and keeping every request it was sent, so
//! a test can read the exact bytes a conversation process put on the wire.
//! td-mail's `tests/mock_fetch.rs` is the precedent; this one answers from
//! fixtures rather than forwarding to a loopback server, since nothing in
//! a model exchange needs one. No test reaches the network.
//!
//! A request whose body carries a routed marker (`route`) is answered
//! from that marker's own script instead, so two conversations' requests,
//! which may interleave, each get their own replies.
//!
//! A request that asks for a stream gets the service's stream mode: the
//! head ending in `stream`, the body as `chunk N` frames, and an `end` or
//! `error` line, or, for a client that must close it, comment frames as
//! OpenRouter sends while a model is silent until the client does.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const PROTOCOL: &str = "td-fetch 1";
/// The service's bound on one frame.
const MAX_FRAME: usize = 64 * 1024;
/// What a stream left open sends while the client reads on.
const KEEPALIVE: &[u8] = b": OPENROUTER PROCESSING\n\n";

/// How a streamed reply ends after its body.
#[derive(Clone, Debug)]
pub enum Tail {
    /// `end`: the body is whole.
    End,
    /// `error kind: reason`: the origin broke off.
    Error(String),
    /// Comment frames every 20 ms until the client closes the connection,
    /// which the mock counts (`closed`).
    Open,
}

/// One scripted answer.
#[derive(Clone, Debug)]
pub enum Reply {
    /// A counted reply: status, headers and body. A request for a stream
    /// gets it as the service would, framed, and ended.
    Http {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    /// A streamed reply: its body in frames of `frame` bytes, then `tail`.
    Stream {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        frame: usize,
        tail: Tail,
    },
    /// The service's `error kind: reason` head.
    Error(String),
    /// The connection held open with no answer until the mock is dropped:
    /// a request in flight when its process is killed.
    Hang,
}

impl Reply {
    /// A 200 event stream from fixture `name`, in frames of 61 bytes so
    /// that its events straddle them.
    pub fn sse(name: &str) -> Self {
        Self::Stream {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: fixture(name),
            frame: 61,
            tail: Tail::End,
        }
    }

    /// The same, its lines ending in CRLF.
    pub fn sse_crlf(name: &str) -> Self {
        let text = String::from_utf8(fixture(name)).expect("a UTF-8 fixture");
        Self::sse(name).body(text.replace('\n', "\r\n").into_bytes())
    }

    fn body(self, new: Vec<u8>) -> Self {
        match self {
            Self::Stream {
                status,
                headers,
                frame,
                tail,
                ..
            } => Self::Stream {
                status,
                headers,
                body: new,
                frame,
                tail,
            },
            other => other,
        }
    }

    /// A stream cut off after the event that carries `marker`.
    pub fn cut_after(self, marker: &str) -> Self {
        let Self::Stream { ref body, .. } = self else {
            return self;
        };
        let text = String::from_utf8(body.clone()).expect("a UTF-8 fixture");
        let at = text.find(marker).expect("the marker");
        let end = text[at..].find("\n\n").expect("the event's end") + at + 2;
        self.body(text.as_bytes()[..end].to_vec())
    }

    /// The same stream with `data: [DONE]` after what it has.
    pub fn done(self) -> Self {
        let Self::Stream { ref body, .. } = self else {
            return self;
        };
        let mut body = body.clone();
        body.extend_from_slice(b"data: [DONE]\n\n");
        self.body(body)
    }

    /// A stream ending `tail`.
    pub fn tail(self, new: Tail) -> Self {
        match self {
            Self::Stream {
                status,
                headers,
                body,
                frame,
                ..
            } => Self::Stream {
                status,
                headers,
                body,
                frame,
                tail: new,
            },
            other => other,
        }
    }

    /// A 200 with a fixture's body.
    pub fn ok(fixture: &str) -> Self {
        Self::status(200, fixture)
    }

    /// A reply of `status` whose body is fixture `name` from
    /// `tests/fixtures/openrouter/`.
    pub fn status(status: u16, name: &str) -> Self {
        Self::Http {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: fixture(name),
        }
    }

    pub fn with_header(self, name: &str, value: &str) -> Self {
        match self {
            Self::Http {
                status,
                mut headers,
                body,
            } => {
                headers.push((name.into(), value.into()));
                Self::Http {
                    status,
                    headers,
                    body,
                }
            }
            other => other,
        }
    }
}

/// A recorded exchange's bytes.
pub fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/openrouter")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// A request as the client wrote it.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub limit: Option<u64>,
    pub redirects: Option<u32>,
    pub stream: bool,
    pub body: Vec<u8>,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8(self.body.clone()).expect("a UTF-8 body")
    }
}

/// Scripts by the marker a request's body carries, each in reverse.
type Routed = Mutex<Vec<(String, Vec<Reply>)>>;
type Routes = Arc<Routed>;

pub struct MockFetch {
    runtime: PathBuf,
    recorded: Arc<Mutex<Vec<Recorded>>>,
    script: Arc<Mutex<Vec<Reply>>>,
    routes: Routes,
    stop: Arc<AtomicBool>,
    /// Open streams their client closed.
    closed: Arc<AtomicUsize>,
    handle: Option<JoinHandle<()>>,
}

impl MockFetch {
    /// Serves at `runtime/td-fetch/socket`, answering in `script`'s order;
    /// past its end each request gets a transport error.
    pub fn start(runtime: &Path, script: Vec<Reply>) -> Self {
        let dir = runtime.join("td-fetch");
        std::fs::create_dir_all(&dir).expect("the td-fetch directory");
        let path = dir.join("socket");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind the mock fetch socket");
        listener
            .set_nonblocking(true)
            .expect("a nonblocking listener");
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let script = Arc::new(Mutex::new(script.into_iter().rev().collect::<Vec<_>>()));
        let routes: Routes = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicUsize::new(0));
        let handle = {
            let (recorded, script, routes, stop, closed) = (
                recorded.clone(),
                script.clone(),
                routes.clone(),
                stop.clone(),
                closed.clone(),
            );
            thread::spawn(move || {
                let mut held = Vec::new();
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let scripts = (&*script, &*routes);
                            if let Some(stream) = serve(stream, &recorded, scripts, &stop, &closed)
                            {
                                held.push(stream);
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Self {
            runtime: runtime.to_path_buf(),
            recorded,
            script,
            routes,
            stop,
            closed,
            handle: Some(handle),
        }
    }

    /// Waits until the client has closed `count` open streams.
    pub fn wait_closed(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.closed.load(Ordering::SeqCst) < count {
            assert!(
                Instant::now() < deadline,
                "{} of {count} open streams were closed",
                self.closed.load(Ordering::SeqCst)
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn runtime(&self) -> &Path {
        &self.runtime
    }

    /// Every request so far.
    pub fn requests(&self) -> Vec<Recorded> {
        self.recorded.lock().expect("the record").clone()
    }

    /// Adds replies after those still scripted.
    pub fn then(&self, replies: Vec<Reply>) {
        let mut script = self.script.lock().expect("the script");
        for reply in replies {
            script.insert(0, reply);
        }
    }

    /// Answers each request whose body carries `marker` with the next of
    /// `replies`, before the main script, until they run out.
    pub fn route(&self, marker: &str, replies: Vec<Reply>) {
        let mut routes = self.routes.lock().expect("the routes");
        routes.push((marker.into(), replies.into_iter().rev().collect()));
    }

    /// Waits until `count` requests have come.
    pub fn wait_for(&self, count: usize) -> Vec<Recorded> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let requests = self.requests();
            if requests.len() >= count {
                return requests;
            }
            assert!(
                Instant::now() < deadline,
                "{} of {count} requests came",
                requests.len()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for MockFetch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The head of a streamed reply.
fn stream_head(status: u16, headers: &[(String, String)]) -> Vec<u8> {
    let mut head = format!("{PROTOCOL}\nstatus {status}\n");
    for (name, value) in headers {
        head.push_str(&format!("header {name}: {value}\n"));
    }
    head.push_str("stream\n\n");
    head.into_bytes()
}

/// `body` as frames of at most `frame` bytes.
fn frames(body: &[u8], frame: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for piece in body.chunks(frame.clamp(1, MAX_FRAME)) {
        out.extend(format!("chunk {}\n", piece.len()).into_bytes());
        out.extend(piece);
    }
    out
}

/// Comment frames until the client closes the connection, counted then.
fn keep_open(stream: UnixStream, stop: Arc<AtomicBool>, closed: Arc<AtomicUsize>) {
    let mut frame = format!("chunk {}\n", KEEPALIVE.len()).into_bytes();
    frame.extend(KEEPALIVE);
    let deadline = Instant::now() + Duration::from_secs(20);
    while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
        if (&stream).write_all(&frame).is_err() {
            closed.fetch_add(1, Ordering::SeqCst);
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Reads one request, records it and answers it; a held connection is
/// handed back to be kept open.
fn serve(
    stream: UnixStream,
    recorded: &Mutex<Vec<Recorded>>,
    (script, routes): (&Mutex<Vec<Reply>>, &Routed),
    stop: &Arc<AtomicBool>,
    closed: &Arc<AtomicUsize>,
) -> Option<UnixStream> {
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let request = match read_request(&mut reader) {
        Ok(request) => request,
        Err(e) => {
            let _ = (&stream).write_all(format!("{PROTOCOL}\nerror malformed: {e}\n\n").as_bytes());
            return None;
        }
    };
    let streamed = request.stream;
    let text = String::from_utf8_lossy(&request.body).into_owned();
    recorded.lock().ok()?.push(request);
    let routed = routes
        .lock()
        .ok()?
        .iter_mut()
        .find(|(marker, replies)| !replies.is_empty() && text.contains(marker.as_str()))
        .and_then(|(_, replies)| replies.pop());
    let reply = match routed {
        Some(reply) => Some(reply),
        None => script.lock().ok()?.pop(),
    };
    let bytes = match reply {
        // As the service frames any reply to a request for a stream.
        Some(Reply::Http {
            status,
            headers,
            body,
        }) if streamed => {
            let mut bytes = stream_head(status, &headers);
            bytes.extend(frames(&body, MAX_FRAME));
            bytes.extend(b"end\n");
            bytes
        }
        Some(Reply::Stream {
            status,
            headers,
            body,
            frame,
            tail,
        }) if streamed => {
            let mut bytes = stream_head(status, &headers);
            bytes.extend(frames(&body, frame));
            match tail {
                Tail::End => bytes.extend(b"end\n"),
                Tail::Error(line) => bytes.extend(format!("error {line}\n").into_bytes()),
                Tail::Open => {
                    (&stream).write_all(&bytes).ok()?;
                    let (stop, closed) = (stop.clone(), closed.clone());
                    thread::spawn(move || keep_open(stream, stop, closed));
                    return None;
                }
            }
            bytes
        }
        Some(Reply::Stream { .. }) => {
            format!("{PROTOCOL}\nerror malformed: a stream scripted for a counted request\n\n")
                .into_bytes()
        }
        Some(Reply::Http {
            status,
            headers,
            body,
        }) => {
            let mut head = format!("{PROTOCOL}\nstatus {status}\n");
            for (name, value) in headers {
                head.push_str(&format!("header {name}: {value}\n"));
            }
            head.push_str(&format!("body {}\n\n", body.len()));
            let mut bytes = head.into_bytes();
            bytes.extend(body);
            bytes
        }
        Some(Reply::Error(line)) => format!("{PROTOCOL}\nerror {line}\n\n").into_bytes(),
        Some(Reply::Hang) => return Some(stream),
        None => format!("{PROTOCOL}\nerror transport: no scripted reply\n\n").into_bytes(),
    };
    let _ = (&stream).write_all(&bytes);
    let _ = stream.shutdown(std::net::Shutdown::Write);
    None
}

fn read_line(reader: &mut impl BufRead) -> Result<String, String> {
    let mut line = String::new();
    if reader.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
        return Err("closed before the head ended".into());
    }
    Ok(line.trim_end_matches('\n').to_string())
}

fn read_request(reader: &mut impl BufRead) -> Result<Recorded, String> {
    if read_line(reader)? != PROTOCOL {
        return Err("not a td-fetch request".into());
    }
    let mut request = Recorded {
        method: String::new(),
        url: String::new(),
        headers: Vec::new(),
        limit: None,
        redirects: None,
        stream: false,
        body: Vec::new(),
    };
    let mut length = 0usize;
    loop {
        let line = read_line(reader)?;
        if line.is_empty() {
            break;
        }
        let (key, value) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        match key {
            "method" => request.method = value.into(),
            "url" => request.url = value.into(),
            "header" => {
                let (name, value) = value.split_once(": ").ok_or("a header line")?;
                request.headers.push((name.into(), value.into()));
            }
            "limit" => request.limit = value.parse().ok(),
            "redirects" => request.redirects = value.parse().ok(),
            "stream" => request.stream = true,
            "body" => length = value.parse().map_err(|_| "a body length")?,
            other => return Err(format!("head key {other:?}")),
        }
    }
    request.body = vec![0u8; length];
    reader
        .read_exact(&mut request.body)
        .map_err(|e| e.to_string())?;
    Ok(request)
}
