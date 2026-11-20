//! Interactive mail change hints through td-fetch's bounded streaming pool.
use super::client::JmapClient;
use crate::backend::BackendResponse;
use std::os::unix::net::UnixStream;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::Sender,
        Condvar, Mutex, MutexGuard,
    },
    thread,
    time::{Duration, Instant},
};

const MAX_EVENT: usize = 8192;
pub(super) const MAX_URL: usize = 4096;
const STREAM_LIMIT: u64 = 512 * 1024;
const MIN_RETRY: Duration = Duration::from_secs(2);
const MAX_RETRY: Duration = Duration::from_secs(60);

pub(crate) struct Source {
    url: String,
    auth: String,
    account: String,
}

impl Source {
    pub(super) fn new(
        configured: &str,
        discovery: &str,
        template: &str,
        auth: String,
        account: &str,
    ) -> Option<Self> {
        // A session obtained after a cross-origin redirect is untrusted even
        // if it advertises an event URL at the configured site.
        let origin = JmapClient::origin(configured)?;
        if !origin.starts_with("https://")
            || template.len() > MAX_URL
            || template.bytes().any(|b| !b.is_ascii_graphic())
            || template.contains(['\\', '#'])
            || JmapClient::origin(discovery).as_ref() != Some(&origin)
            || JmapClient::origin(template).as_ref() != Some(&origin)
            || !super::client::valid_id(account)
        {
            return None;
        }
        let mut url = String::new();
        let mut rest = template;
        let mut seen = 0;
        while let Some(i) = rest.find(['{', '}']) {
            td_alloc::push_str(&mut url, rest.get(..i)?).ok()?;
            let (name, remaining) = rest.get(i..)?.strip_prefix('{')?.split_once('}')?;
            let (bit, value) = match name {
                "types" => (1, "Mailbox%2CEmail"),
                "closeafter" => (2, "no"),
                "ping" => (4, "30"),
                _ => return None,
            };
            if seen & bit != 0 {
                return None;
            }
            seen |= bit;
            td_alloc::push_str(&mut url, value).ok()?;
            rest = remaining;
        }
        td_alloc::push_str(&mut url, rest).ok()?;
        (seen == 7 && url.len() <= MAX_URL).then_some(Self {
            url,
            auth,
            account: td_alloc::string(account).ok()?,
        })
    }

    fn read(&self, control: &Control, response: &Sender<BackendResponse>) {
        let stream = td_fetch_client::get_stream_with_connector(
            &self.url,
            &[
                ("authorization", &self.auth),
                ("accept", "text/event-stream"),
            ],
            STREAM_LIMIT,
            &|path| {
                let socket =
                    td_fs::unix_connect_until(&path, Instant::now() + Duration::from_secs(1))?;
                let mut state = control.lock();
                if state.stopped {
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                state.socket = Some(socket.try_clone()?);
                Ok(socket)
            },
        );
        let Ok(mut stream) = stream else { return };
        if stream.status != 200
            || !stream.header("content-type").is_some_and(|value| {
                value
                    .split(';')
                    .next()
                    .is_some_and(|media| media.trim().eq_ignore_ascii_case("text/event-stream"))
            })
        {
            return;
        }
        // Every successful (re)connection fetches current mail through the
        // ordinary authenticated methods, including changes during a gap.
        control.notify(response);
        let mut events = Events::default();
        while let Ok(Some(chunk)) = stream.next_chunk() {
            if !events.consume(chunk, &self.account, || control.notify(response)) {
                break;
            }
        }
    }
}

#[derive(Default)]
struct Events {
    line: Vec<u8>,
    data: Vec<u8>,
    is_state: bool,
    after_cr: bool,
    size: usize,
}

impl Events {
    fn consume(&mut self, bytes: &[u8], account: &str, mut notify: impl FnMut()) -> bool {
        for &byte in bytes {
            if std::mem::take(&mut self.after_cr) && byte == b'\n' {
                continue;
            }
            self.size += 1;
            if self.size > MAX_EVENT {
                return false;
            }
            if !matches!(byte, b'\n' | b'\r') {
                if td_alloc::push(&mut self.line, byte).is_err() {
                    return false;
                }
                continue;
            }
            self.after_cr = byte == b'\r';
            if self.line.is_empty() {
                if self.is_state
                    && td_json::parse_slice(&self.data).ok().is_some_and(|json| {
                        json.get("@type").and_then(td_json::Json::as_str) == Some("StateChange")
                            && json
                                .get("changed")
                                .and_then(|changed| changed.get(account))
                                .is_some_and(|types| {
                                    ["Email", "Mailbox"].iter().any(|name| {
                                        types
                                            .get(name)
                                            .and_then(td_json::Json::as_str)
                                            .is_some_and(|token| !token.is_empty())
                                    })
                                })
                    })
                {
                    notify();
                }
                self.is_state = false;
                self.data.clear();
                self.size = 0;
            } else if let Some(value) = self.line.strip_prefix(b"event:") {
                self.is_state = value.strip_prefix(b" ").unwrap_or(value) == b"state";
            } else if let Some(value) = self.line.strip_prefix(b"data:") {
                if td_alloc::extend_from_slice(
                    &mut self.data,
                    value.strip_prefix(b" ").unwrap_or(value),
                )
                .is_err()
                    || td_alloc::push(&mut self.data, b'\n').is_err()
                {
                    return false;
                }
            }
            self.line.clear();
        }
        true
    }
}

#[derive(Default)]
struct State {
    socket: Option<UnixStream>,
    notified: bool,
    stopped: bool,
}

#[derive(Default)]
struct Control {
    state: Mutex<State>,
    changed: Condvar,
}

impl Control {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn notify(&self, response: &Sender<BackendResponse>) {
        let mut state = self.lock();
        if !state.stopped && !state.notified {
            state.notified = response.send(BackendResponse::MailboxStateChanged).is_ok();
        }
    }

    fn watch(&self, source: Source, response: &Sender<BackendResponse>) {
        let mut retry = MIN_RETRY;
        loop {
            let started = Instant::now();
            source.read(self, response);
            let mut state = self.lock();
            state.socket.take();
            if state.stopped {
                return;
            }
            if started.elapsed() >= MAX_RETRY {
                retry = MIN_RETRY;
            }
            let (state, _) = self
                .changed
                .wait_timeout_while(state, retry, |s| !s.stopped)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.stopped {
                return;
            }
            retry = (retry * 2).min(MAX_RETRY);
        }
    }
}

const MAX_WATCHERS: usize = 2;
static WATCHERS: AtomicUsize = AtomicUsize::new(0);

struct Permit;
impl Permit {
    fn acquire() -> Option<Self> {
        WATCHERS
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < MAX_WATCHERS).then_some(n + 1)
            })
            .ok()
            .map(|_| Self)
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        WATCHERS.fetch_sub(1, Ordering::Relaxed);
    }
}

pub(crate) struct Watch<'s, 'env> {
    control: &'s Control,
    response: &'s Sender<BackendResponse>,
    scope: &'s thread::Scope<'s, 'env>,
    worker: Option<thread::ScopedJoinHandle<'s, ()>>,
}

impl Watch<'_, '_> {
    fn stop(&mut self) {
        if let Some(worker) = self.worker.take() {
            {
                let mut state = self.control.lock();
                state.stopped = true;
                if let Some(socket) = state.socket.take() {
                    let _ = socket.shutdown(std::net::Shutdown::Both);
                }
                self.control.changed.notify_one();
            }
            let _ = thread::ScopedJoinHandle::join(worker);
        }
    }

    pub(crate) fn select(&mut self, source: Option<Source>) {
        self.stop();
        let Some(source) = source else { return };
        let Some(permit) = Permit::acquire() else {
            return;
        };
        let control = self.control;
        let response = self.response;
        *control.lock() = State::default();
        self.worker = thread::Builder::new()
            .spawn_scoped(self.scope, move || {
                let _permit = permit;
                control.watch(source, response)
            })
            .ok();
    }

    pub(crate) fn refresh_started(&self) {
        self.control.lock().notified = false;
    }
}

impl Drop for Watch<'_, '_> {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(crate) fn during(
    enabled: bool,
    response: &Sender<BackendResponse>,
    serve: impl FnOnce(Option<&mut Watch<'_, '_>>),
) {
    if !enabled {
        return serve(None);
    }
    let control = Control::default();
    thread::scope(|scope| {
        let mut watch = Watch {
            control: &control,
            response,
            scope,
            worker: None,
        };
        serve(Some(&mut watch));
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::{
        ffi::OsString,
        io::{BufRead, BufReader, Read, Write},
        os::unix::net::UnixListener,
        sync::mpsc,
    };

    const ORIGIN: &str = "https://example.test";
    const TEMPLATE: &str =
        "https://example.test/events?types={types}&closeafter={closeafter}&ping={ping}";
    const CHANGE: &[u8] = b"event: state\r\nid: first\r\ndata: {\"@type\":\"StateChange\",\r\ndata: \"changed\":{\"a1\":{\"Email\":\"v2\"}}}\r\n\r\n";
    const WAIT: Duration = Duration::from_secs(7);

    fn source() -> Source {
        Source::new(ORIGIN, ORIGIN, TEMPLATE, "Basic dTpw".into(), "a1").unwrap()
    }

    #[test]
    fn event_credentials_and_template_expansion_stay_at_the_discovery_origin() {
        assert_eq!(
            source().url,
            "https://example.test/events?types=Mailbox%2CEmail&closeafter=no&ping=30"
        );
        for (discovery, template) in [
            ("https://other.test", TEMPLATE),
            (ORIGIN, "https://other.test/?{types}/{closeafter}/{ping}"),
            (ORIGIN, "http://example.test/?{types}/{closeafter}/{ping}"),
            (
                ORIGIN,
                "https://example.test@elsewhere.test/?{types}/{closeafter}/{ping}",
            ),
            (
                ORIGIN,
                "https://example.test/?{types}/{closeafter}/{unknown}",
            ),
            (ORIGIN, "https://example.test/?{types}/{closeafter}/{types}"),
            (
                ORIGIN,
                "https://example.test/?{types}/{closeafter}/{ping}#fragment",
            ),
        ] {
            assert!(Source::new(ORIGIN, discovery, template, String::new(), "a1").is_none());
        }
    }

    #[test]
    fn fragmented_state_frames_match_the_selected_account_and_types() {
        let mut parsed = Events::default();
        let mut hints = 0;
        for event in [
            &b"event: ping\ndata: {}\n\n"[..],
            b"event: state\ndata: {\"@type\":\"StateChange\",\"changed\":{\"else\":{\"Email\":\"v2\"}}}\n\n",
            b"event: state\ndata: {\"@type\":\"StateChange\",\"changed\":{\"a1\":{\"Identity\":\"v2\"}}}\n\n",
            CHANGE,
        ] {
            for byte in event {
                assert!(parsed.consume(&[*byte], "a1", || hints += 1));
            }
        }
        assert_eq!(hints, 1);
        assert!(parsed.consume(&[b'x'; MAX_EVENT], "a1", || hints += 1));
        assert!(!parsed.consume(b"x", "a1", || hints += 1));
    }

    struct RestoreRuntime(Option<OsString>);
    impl Drop for RestoreRuntime {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var("XDG_RUNTIME_DIR", value),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
        }
    }

    #[allow(clippy::panic)]
    fn accept_request(listener: &UnixListener) -> (UnixStream, String) {
        let deadline = Instant::now() + WAIT;
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(5));
                }
                other => panic!("fetch connection: {other:?}"),
            }
        };
        socket.set_read_timeout(Some(WAIT)).unwrap();
        let mut head = String::new();
        let mut reader = BufReader::new(&mut socket);
        while !head.ends_with("\n\n") {
            assert!(reader.read_line(&mut head).unwrap() > 0);
        }
        (socket, head)
    }

    fn accept_fetch(listener: &UnixListener) -> UnixStream {
        let (socket, head) = accept_request(listener);
        assert!(head.contains("\nmethod GET\nurl https://example.test/events?types=Mailbox%2CEmail&closeafter=no&ping=30\n"));
        assert!(head.contains("\nheader authorization: Basic dTpw\n"));
        assert!(head.contains("\nredirects 0\n"));
        assert!(head.contains("\nstream\n"));
        socket
    }

    #[test]
    fn fetch_watch_catches_up_reconnects_and_cancels_header_and_body_waits() {
        let _env = crate::testing::env_lock();
        let dir = td_fs::TempDir::new("mail-watch").unwrap();
        let _restore = RestoreRuntime(std::env::var_os("XDG_RUNTIME_DIR"));
        std::env::set_var("XDG_RUNTIME_DIR", dir.path());
        let path = dir.path().join("td-fetch");
        std::fs::create_dir(&path).unwrap();
        let listener = UnixListener::bind(path.join("socket")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let reply = b"td-fetch 1\nstatus 200\nheader content-type: text/event-stream; charset=utf-8\nstream\n\n";
        let (tx, rx) = mpsc::channel();
        let hint = || {
            assert!(matches!(
                rx.recv_timeout(WAIT),
                Ok(BackendResponse::MailboxStateChanged)
            ))
        };

        let discovered = thread::scope(|scope| {
            let discover = scope.spawn(|| {
                JmapClient::discover(
                    "https://example.test/.well-known/jmap",
                    "u",
                    "p",
                    None,
                    None,
                )
            });
            let (mut socket, head) = accept_request(&listener);
            assert!(head.contains("url https://example.test/.well-known/jmap\n"));
            assert!(head.contains("header authorization: Basic dTpw\n"));
            let session = td_json::json!({
                "username":"u", "apiUrl":"https://example.test/api",
                "eventSourceUrl":TEMPLATE, "accounts":{"a1":{"name":"Inbox"}}
            })
            .to_string();
            write!(
                socket,
                "td-fetch 1\nstatus 200\nbody {}\n\n{}",
                session.len(),
                session
            )
            .unwrap();
            let (_, client) = discover.join().unwrap().unwrap();
            client.events.unwrap()
        });
        during(true, &tx, |watch| {
            let watch = watch.unwrap();
            watch.select(Some(discovered));
            let mut stream = accept_fetch(&listener);
            stream.write_all(reply).unwrap();
            hint();
            watch.refresh_started();
            for part in CHANGE.chunks(17) {
                writeln!(stream, "chunk {}", part.len()).unwrap();
                stream.write_all(part).unwrap();
            }
            hint();
            watch.refresh_started();
            drop(stream);
            let mut resumed = accept_fetch(&listener);
            resumed.write_all(reply).unwrap();
            hint();
            watch.select(Some(source()));
            assert_eq!(resumed.read(&mut [0u8]).unwrap(), 0);
            let mut opening = accept_fetch(&listener);
            watch.select(None);
            assert_eq!(opening.read(&mut [0u8]).unwrap(), 0);
        });
    }
}
