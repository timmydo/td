//! A td-fetch socket for the offline tests (APPLICATIONS.md §W.8): the
//! protocol td-agent's client speaks to td-fetchd, served from a thread at
//! `RUNTIME/td-fetch/socket`, answering each request with the next of a
//! script of recorded exchanges and keeping every request it was sent, so
//! a test can read the exact bytes a conversation process put on the wire.
//! td-mail's `tests/mock_fetch.rs` is the precedent; this one answers from
//! fixtures rather than forwarding to a loopback server, since nothing in
//! a model exchange needs one. No test reaches the network.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const PROTOCOL: &str = "td-fetch 1";

/// One scripted answer.
#[derive(Clone, Debug)]
pub enum Reply {
    /// A counted reply: status, headers and body.
    Http {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    /// The service's `error kind: reason` head.
    Error(String),
    /// The connection held open with no answer until the mock is dropped:
    /// a request in flight when its process is killed.
    Hang,
}

impl Reply {
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

pub struct MockFetch {
    runtime: PathBuf,
    recorded: Arc<Mutex<Vec<Recorded>>>,
    script: Arc<Mutex<Vec<Reply>>>,
    stop: Arc<AtomicBool>,
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
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let (recorded, script, stop) = (recorded.clone(), script.clone(), stop.clone());
            thread::spawn(move || {
                let mut held = Vec::new();
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if let Some(stream) = serve(stream, &recorded, &script) {
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
            stop,
            handle: Some(handle),
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

/// Reads one request, records it and answers it; a held connection is
/// handed back to be kept open.
fn serve(
    stream: UnixStream,
    recorded: &Mutex<Vec<Recorded>>,
    script: &Mutex<Vec<Reply>>,
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
    recorded.lock().ok()?.push(request);
    let reply = script.lock().ok()?.pop();
    let bytes = match reply {
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
