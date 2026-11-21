//! A td-fetch socket for the native cases: the protocol td-web's client
//! speaks to td-fetchd (`td-fetch 1`, a head of `method`, `url`, `header`,
//! `limit`, `redirects`, `public`, `deadline` and `body N` lines, then the
//! body; a reply of `status`, `header` and `body N` lines and the bytes),
//! served from a thread at `RUNTIME/td-fetch/socket` and answered from a
//! fixed table of URLs, a `404` for any other. td-mail's
//! `tests/mock_fetch.rs` is the precedent. No test reaches the network.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const PROTOCOL: &str = "td-fetch 1";

/// One URL's answer: its type and body.
pub type Page = (&'static str, &'static str, Vec<u8>);

pub struct MockFetch {
    stop: Arc<AtomicBool>,
    asked: Arc<Mutex<Vec<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl MockFetch {
    /// Serves `pages` at `runtime/td-fetch/socket`; the test hands
    /// `runtime` to td-web as `XDG_RUNTIME_DIR`.
    pub fn start(runtime: &Path, pages: Vec<Page>) -> Self {
        let directory = runtime.join("td-fetch");
        std::fs::create_dir(&directory).unwrap();
        let listener = UnixListener::bind(directory.join("socket")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let asked = Arc::new(Mutex::new(Vec::new()));
        let pages = Arc::new(pages);
        let thread = {
            let (stop, asked) = (Arc::clone(&stop), Arc::clone(&asked));
            thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let (pages, asked) = (Arc::clone(&pages), Arc::clone(&asked));
                            thread::spawn(move || serve(stream, &pages, &asked));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Self {
            stop,
            asked,
            thread: Some(thread),
        }
    }

    /// The URLs asked so far, in the order they came.
    pub fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

impl Drop for MockFetch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(stream: UnixStream, pages: &[Page], asked: &Mutex<Vec<String>>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let reply = match url_of(&mut reader) {
        Ok(url) => {
            asked.lock().unwrap().push(url.clone());
            let (status, kind, body) = match pages.iter().find(|(at, _, _)| *at == url) {
                Some((_, kind, body)) => (200, *kind, body.as_slice()),
                None => (404, "text/plain", &b"not here"[..]),
            };
            let mut head =
                format!("{PROTOCOL}\nstatus {status}\nheader content-type: {kind}\n").into_bytes();
            head.extend_from_slice(format!("body {}\n\n", body.len()).as_bytes());
            head.extend_from_slice(body);
            head
        }
        Err(why) => format!("{PROTOCOL}\nerror malformed: {why}\n\n").into_bytes(),
    };
    let _ = writer.write_all(&reply);
    let _ = writer.flush();
    let _ = writer.shutdown(std::net::Shutdown::Write);
}

/// The request's URL, its head and body read whole; only GET is served.
fn url_of(reader: &mut impl BufRead) -> Result<String, String> {
    let mut line = String::new();
    let mut next = |line: &mut String| -> Result<(), String> {
        line.clear();
        match reader.read_line(line) {
            Ok(0) => Err("closed before the head ended".into()),
            Ok(_) => {
                line.truncate(line.trim_end_matches('\n').len());
                Ok(())
            }
            Err(error) => Err(format!("read: {error}")),
        }
    };
    next(&mut line)?;
    if line != PROTOCOL {
        return Err("not a td-fetch request".into());
    }
    let (mut url, mut get, mut body) = (None, false, 0usize);
    loop {
        next(&mut line)?;
        if line.is_empty() {
            break;
        }
        let (key, value) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        match key {
            "method" => get = value == "GET",
            "url" => url = Some(value.to_string()),
            "body" => body = value.parse().map_err(|_| format!("body {value:?}"))?,
            "header" | "limit" | "redirects" | "public" | "deadline" => {}
            other => return Err(format!("head key {other:?}")),
        }
    }
    let mut rest = vec![0u8; body];
    reader
        .read_exact(&mut rest)
        .map_err(|error| format!("body: {error}"))?;
    if !get {
        return Err("only GET is served".into());
    }
    url.ok_or_else(|| "no url".into())
}
