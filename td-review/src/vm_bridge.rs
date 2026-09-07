use crate::vm_wire as wire;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, String>;
const RELAY_TIMEOUT: Duration = wire::TIMEOUT.saturating_add(Duration::from_secs(2));

fn request_id() -> Result<u64> {
    let mut bytes = [0; 8];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|e| format!("VM request identity: {e}"))?;
    Ok(u64::from_le_bytes(bytes).max(1))
}

fn connect(path: &Path) -> Result<UnixStream> {
    let stream = UnixStream::connect(path).map_err(|e| format!("VM bridge is unavailable: {e}"))?;
    stream.set_nonblocking(true).map_err(|e| e.to_string())?;
    Ok(stream)
}

fn reply(
    stream: &mut UnixStream,
    request: &wire::Message,
    deadline: Instant,
) -> Result<wire::Message> {
    wire::write_all(stream, &request.encode()?, deadline)?;
    let reply = wire::receive(stream, Some(request.id), deadline)?;
    match reply.verb.as_str() {
        wire::OK => {
            let shape = match request.verb.as_str() {
                wire::SNAPSHOT => reply.revision != 0 && reply.data == b"clipboard-v1 feed-v1",
                wire::GET => {
                    reply.revision == request.revision
                        && !reply.data.is_empty()
                        && wire::text(&reply.data).is_ok()
                }
                wire::PUT | wire::FEED => {
                    reply.revision == request.revision && reply.data.is_empty()
                }
                _ => false,
            };
            if !shape {
                return Err("VM reply does not match its request".into());
            }
            Ok(reply)
        }
        wire::ERROR => Err(String::from_utf8_lossy(&reply.data)
            .chars()
            .take(256)
            .collect()),
        _ => Err("guest sent a request instead of a reply".into()),
    }
}

pub fn ask(dir: &Path, verb: &str, data: Vec<u8>) -> Result<Vec<u8>> {
    let mut stream = connect(&dir.join("bridge"))?;
    let request = wire::Message::new(request_id()?, verb, 0, data);
    let response = reply(
        &mut stream,
        &request,
        Instant::now() + RELAY_TIMEOUT + Duration::from_secs(2),
    )?;
    Ok(response.data)
}

fn guest(dir: &Path, request: wire::Message, deadline: Instant) -> Result<wire::Message> {
    let mut stream = connect(&dir.join("guest"))?;
    // Separate old bytes from this request; declared lengths reject truncated payloads.
    wire::write_all(&mut stream, b"\n", deadline)?;
    let mut request = request;
    if matches!(request.verb.as_str(), wire::PUT | wire::GET) {
        let snapshot = reply(
            &mut stream,
            &wire::Message::new(request.id, wire::SNAPSHOT, 0, Vec::new()),
            deadline,
        )?;
        request.revision = snapshot.revision;
    }
    reply(&mut stream, &request, deadline)
}

fn enabled(dir: &Path) -> Result<bool> {
    let file = match File::open(dir.join("clipboard")) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(e) => return Err(format!("read clipboard sharing setting: {e}")),
    };
    let mut value = String::new();
    file.take(4)
        .read_to_string(&mut value)
        .map_err(|e| e.to_string())?;
    match value.as_str() {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err("invalid clipboard sharing setting".into()),
    }
}

fn publish(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let temp = dir.join(format!("{name}-{:x}.tmp", request_id()?));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|e| format!("stage VM setting: {e}"))?;
    let result = (|| {
        file.write_all(bytes).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        fs::rename(&temp, dir.join(name)).map_err(|e| format!("publish VM setting: {e}"))
    })();
    let _ = fs::remove_file(temp);
    result
}

pub fn sharing(dir: &Path, mode: &str) -> Result<()> {
    if !matches!(mode, "on" | "off") {
        return Err("sharing must be on or off".into());
    }
    publish(dir, "clipboard", mode.as_bytes())?;
    println!("Explicit clipboard transfers: {mode}");
    Ok(())
}

pub fn configure_feed(dir: &Path, port: &str) -> Result<()> {
    let endpoint = if port == "off" {
        String::new()
    } else {
        format!("http://10.0.2.2:{port}")
    };
    wire::feed(endpoint.as_bytes())?;
    publish(dir, "feed", endpoint.as_bytes())?;
    match ask(dir, wire::FEED, endpoint.into_bytes()) {
        Ok(_) => println!("Guest feed configuration applied"),
        Err(error) => return Err(format!("Feed configuration saved; guest confirmation unavailable: {error}. The supervisor will retry.")),
    }
    Ok(())
}

pub struct Supervisor {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Supervisor {
    pub fn start(dir: &Path) -> Result<Self> {
        let listener = UnixListener::bind(dir.join("bridge"))
            .map_err(|e| format!("bind VM supervisor bridge: {e}"))?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let dir = dir.to_path_buf();
        let thread = thread::Builder::new()
            .name("td-vm-bridge".into())
            .spawn(move || serve(listener, dir, stopped))
            .map_err(|e| e.to_string())?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(listener: UnixListener, dir: PathBuf, stop: Arc<AtomicBool>) {
    let mut feed_probe = Instant::now();
    let mut idle = 0u8;
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                idle = 0;
                if stream.set_nonblocking(true).is_err() {
                    continue;
                }
                let deadline = Instant::now() + RELAY_TIMEOUT;
                if let Ok(request) = wire::receive(&mut stream, None, deadline) {
                    let id = request.id;
                    let result = forward(&dir, request, deadline);
                    let response = match result {
                        Ok(response) => response,
                        Err(error) => wire::Message::new(id, wire::ERROR, 0, error.into_bytes()),
                    };
                    if let Ok(bytes) = response.encode() {
                        let _ = wire::write_all(
                            &mut stream,
                            &bytes,
                            Instant::now() + Duration::from_millis(500),
                        );
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                idle = idle.saturating_add(1);
                thread::sleep(Duration::from_millis(if idle < 5 { 10 } else { 100 }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionAborted => continue,
            Err(e) if matches!(e.raw_os_error(), Some(23 | 24)) => {
                // ENFILE/EMFILE can clear without restarting the VM.
                thread::sleep(Duration::from_millis(100));
            }
            Err(_) => break,
        }
        if Instant::now() >= feed_probe {
            feed_probe = Instant::now() + Duration::from_secs(2);
            // Configuration retries are idempotent. Clipboard actions never
            // retry automatically; a timeout can mean the action took effect.
            if let Ok(file) = File::open(dir.join("feed")) {
                let mut bytes = Vec::new();
                if file.take(129).read_to_end(&mut bytes).is_ok() && wire::feed(&bytes).is_ok() {
                    if let Ok(id) = request_id() {
                        let request = wire::Message::new(id, wire::FEED, 0, bytes);
                        let _ = guest(&dir, request, Instant::now() + Duration::from_millis(500));
                    }
                }
            }
        }
    }
    let _ = fs::remove_file(dir.join("bridge"));
}

fn forward(dir: &Path, request: wire::Message, deadline: Instant) -> Result<wire::Message> {
    if request.revision != 0 {
        return Err("host requests cannot supply guest revisions".into());
    }
    match request.verb.as_str() {
        wire::PUT => {
            wire::text(&request.data)?;
            if request.data.is_empty() {
                return Err("clipboard import needs nonempty text".into());
            }
        }
        wire::GET | wire::SNAPSHOT if request.data.is_empty() => {}
        wire::FEED => {
            wire::feed(&request.data)?;
        }
        _ => return Err("unsupported host bridge request".into()),
    }
    if matches!(request.verb.as_str(), wire::PUT | wire::GET) && !enabled(dir)? {
        return Err("clipboard sharing is disabled for this instance".into());
    }
    let exporting = request.verb == wire::GET;
    let snapshot = request.verb == wire::SNAPSHOT;
    let mut response = guest(dir, request, deadline)?;
    // The local request has no guest revision; only the relay takes that lease.
    if !snapshot {
        response.revision = 0;
    }
    if exporting && !enabled(dir)? {
        return Err("clipboard sharing was disabled during transfer".into());
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::sync::atomic::AtomicU64;
    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "td-vm-relay-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn supervisor_pins_snapshot_and_routes_only_explicit_requests() {
        let temp = Temp::new();
        let listener = UnixListener::bind(temp.0.join("guest")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let peer = thread::spawn(move || {
            let deadline = Instant::now() + wire::TIMEOUT;
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(pair) => break pair,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(e) => panic!("fake guest accept: {e}"),
                }
            };
            stream.set_nonblocking(true).unwrap();
            let snapshot = wire::receive(&mut stream, None, deadline).unwrap();
            assert_eq!(snapshot.verb, wire::SNAPSHOT);
            assert!(snapshot.data.is_empty());
            wire::write_all(
                &mut stream,
                &wire::Message::new(snapshot.id, wire::OK, 17, b"clipboard-v1 feed-v1".to_vec())
                    .encode()
                    .unwrap(),
                deadline,
            )
            .unwrap();
            let request = wire::receive(&mut stream, None, deadline).unwrap();
            assert_eq!((request.id, request.revision), (snapshot.id, 17));
            assert_eq!(request.verb, wire::PUT);
            assert_eq!(request.data, "hello\n世界".as_bytes());
            wire::write_all(
                &mut stream,
                &wire::Message::new(request.id, wire::OK, 17, Vec::new())
                    .encode()
                    .unwrap(),
                deadline,
            )
            .unwrap();
        });
        let supervisor = Supervisor::start(&temp.0).unwrap();
        assert!(ask(&temp.0, wire::PUT, "hello\n世界".as_bytes().to_vec())
            .unwrap()
            .is_empty());
        peer.join().unwrap();
        sharing(&temp.0, "off").unwrap();
        assert!(ask(&temp.0, wire::GET, Vec::new())
            .unwrap_err()
            .contains("disabled"));
        assert!(ask(&temp.0, wire::ERROR, Vec::new())
            .unwrap_err()
            .contains("unsupported"));
        drop(supervisor);
        assert!(!temp.0.join("bridge").exists());
    }
}
