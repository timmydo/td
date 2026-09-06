//! A td-fetch socket for the integration tests: the protocol the client speaks
//! to td-fetchd (`td-fetch 1`, a head of `method`, `url`, `header`, `limit`,
//! `redirects` and `body N` lines, the body; a reply of `status`, `header` and
//! `body N` lines and the bytes, or an `error` line), served from a thread and
//! answered over plain HTTP/1.1 to the mock server on loopback. No TLS: the
//! tests' URLs are `http://127.0.0.1`. Redirects are followed as the service
//! follows them: none for POST, the client's count or five for GET, and the
//! `authorization` and `cookie` headers dropped on the way.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

const PROTOCOL: &str = "td-fetch 1";
const MAX_REDIRECTS: u32 = 5;

pub struct MockFetchSocket {
    runtime_dir: PathBuf,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MockFetchSocket {
    /// Serve at `runtime_dir/td-fetch/socket`; the test hands `runtime_dir`
    /// to the client as `XDG_RUNTIME_DIR`.
    pub fn start(runtime_dir: &Path) -> Self {
        let dir = runtime_dir.join("td-fetch");
        std::fs::create_dir_all(&dir).expect("create the td-fetch directory");
        let path = dir.join("socket");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind the mock fetch socket");
        listener
            .set_nonblocking(true)
            .expect("nonblocking mock fetch socket");
        let shutdown = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&shutdown);
        let handle = thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        thread::spawn(move || serve_one(stream));
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        MockFetchSocket {
            runtime_dir: runtime_dir.to_path_buf(),
            shutdown,
            handle: Some(handle),
        }
    }

    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }
}

impl Drop for MockFetchSocket {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct Request {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    redirects: Option<u32>,
}

fn serve_one(stream: UnixStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut writer = match stream.try_clone() {
        Ok(writer) => writer,
        Err(_) => return,
    };
    let mut reader = BufReader::new(stream);
    let reply = match read_request(&mut reader) {
        Ok(request) => match perform(&request) {
            Ok((status, headers, body)) => {
                let mut reply = format!("{PROTOCOL}\nstatus {status}\n");
                for (name, value) in &headers {
                    reply.push_str(&format!("header {name}: {value}\n"));
                }
                reply.push_str(&format!("body {}\n\n", body.len()));
                let mut bytes = reply.into_bytes();
                bytes.extend_from_slice(&body);
                bytes
            }
            Err(error) => format!("{PROTOCOL}\nerror transport: {error}\n\n").into_bytes(),
        },
        Err(error) => format!("{PROTOCOL}\nerror malformed: {error}\n\n").into_bytes(),
    };
    let _ = writer.write_all(&reply);
    let _ = writer.flush();
    let _ = writer.shutdown(std::net::Shutdown::Write);
}

fn read_line(reader: &mut impl BufRead) -> Result<String, String> {
    let mut line = String::new();
    let read = reader
        .read_line(&mut line)
        .map_err(|e| format!("read: {e}"))?;
    if read == 0 {
        return Err("closed before the head ended".into());
    }
    Ok(line.trim_end_matches('\n').to_string())
}

fn read_request(reader: &mut impl BufRead) -> Result<Request, String> {
    if read_line(reader)? != PROTOCOL {
        return Err("not a td-fetch request".into());
    }
    let mut method = None;
    let mut url = None;
    let mut headers = Vec::new();
    let mut body_len = 0usize;
    let mut redirects = None;
    loop {
        let line = read_line(reader)?;
        if line.is_empty() {
            break;
        }
        let (key, value) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        match key {
            "method" => method = Some(value.to_string()),
            "url" => url = Some(value.to_string()),
            "header" => {
                let (name, value) = value
                    .split_once(": ")
                    .ok_or_else(|| format!("header line {line:?}"))?;
                headers.push((name.to_string(), value.to_string()));
            }
            "limit" => {}
            "redirects" => {
                redirects = Some(value.parse().map_err(|_| format!("redirects {value:?}"))?);
            }
            "body" => body_len = value.parse().map_err(|_| format!("body {value:?}"))?,
            other => return Err(format!("head key {other:?}")),
        }
    }
    let mut body = vec![0u8; body_len];
    reader
        .read_exact(&mut body)
        .map_err(|e| format!("body: {e}"))?;
    Ok(Request {
        method: method.ok_or("no method")?,
        url: url.ok_or("no url")?,
        headers,
        body,
        redirects,
    })
}

type Answer = (u16, Vec<(String, String)>, Vec<u8>);

fn perform(request: &Request) -> Result<Answer, String> {
    let allowed = if request.method == "POST" {
        0
    } else {
        request.redirects.unwrap_or(MAX_REDIRECTS)
    };
    let mut url = request.url.clone();
    let mut headers = request.headers.clone();
    let mut followed = 0u32;
    loop {
        let (status, reply_headers, body) =
            http_once(&request.method, &url, &headers, &request.body)?;
        if !(300..400).contains(&status) || allowed == 0 {
            return Ok((status, reply_headers, body));
        }
        if followed >= allowed {
            return Err(format!(
                "{url}: Too Many Redirects: reached max redirects ({allowed})"
            ));
        }
        let location = reply_headers
            .iter()
            .find(|(name, _)| name == "location")
            .map(|(_, value)| value.clone())
            .ok_or_else(|| format!("{url}: redirect without a location"))?;
        url = resolve(&url, &location);
        headers.retain(|(name, _)| name != "authorization" && name != "cookie");
        followed += 1;
    }
}

/// Join a `location` against the URL it answered, as ureq does.
fn resolve(base: &str, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.to_string();
    }
    let rest = base.strip_prefix("http://").unwrap_or(base);
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    if let Some(absolute) = location.strip_prefix('/') {
        return format!("http://{authority}/{absolute}");
    }
    let dir = path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
    format!("http://{authority}/{dir}/{location}")
        .replace("//", "/")
        .replacen("http:/", "http://", 1)
}

fn http_once(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<Answer, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("{url}: the mock fetch socket serves http:// alone"))?;
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, "/".to_string()),
    };
    let mut stream = TcpStream::connect(authority).map_err(|e| format!("{url}: connect: {e}"))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .and_then(|()| stream.write_all(body))
        .and_then(|()| stream.flush())
        .map_err(|e| format!("{url}: write: {e}"))?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| format!("{url}: read: {e}"))?;
    let head_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| format!("{url}: no response head"))?;
    let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| format!("{url}: status line {status_line:?}"))?;
    let mut reply_headers = Vec::new();
    let mut content_length = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().to_string();
            if name == "content-length" {
                content_length = value.parse::<usize>().ok();
            }
            reply_headers.push((name, value));
        }
    }
    let body_start = head_end + 4;
    let body = raw.get(body_start..).unwrap_or(&[]);
    let body = match content_length {
        Some(len) if len <= body.len() => body[..len].to_vec(),
        _ => body.to_vec(),
    };
    Ok((status, reply_headers, body))
}
