//! The VM carrier terminates inside the compositor, outside public Wayland.
use crate::{conn, runtime::Runtime, server::TransferEndpoint, vm_wire as wire};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) struct ClipboardWrite {
    pub file: File,
    pub bytes: Arc<Vec<u8>>,
}

pub(crate) fn start(runtime: Arc<Mutex<Runtime>>) -> Result<(), String> {
    let record = Path::new("/run/td-compositor/1000/vm-port");
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(0x20000 | 0x800)
        .open(record)
    {
        Ok(value) => value,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("open VM port assignment: {e}")),
    };
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    let uid = fs::metadata("/proc/self").map_err(|e| e.to_string())?.uid();
    if !metadata.is_file() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err("VM port assignment is not a private compositor file".into());
    }
    let mut path = String::new();
    file.take(129)
        .read_to_string(&mut path)
        .map_err(|e| e.to_string())?;
    if path.len() > 128
        || !path.strip_prefix("/dev/vport").is_some_and(|suffix| {
            suffix.split_once('p').is_some_and(|(a, b)| {
                !a.is_empty()
                    && !b.is_empty()
                    && a.bytes().chain(b.bytes()).all(|b| b.is_ascii_digit())
            })
        })
    {
        return Err("invalid assigned VM port path".into());
    }
    // Refuse final symlinks and validate the opened object before any I/O.
    let port = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(0x20000 | 0x800)
        .open(path)
        .map_err(|e| format!("open assigned VM port: {e}"))?;
    let meta = port
        .metadata()
        .map_err(|e| format!("inspect VM port: {e}"))?;
    if !meta.file_type().is_char_device() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err("VM port is not a private compositor character device".into());
    }
    let (sender, receiver) = mpsc::sync_channel::<ClipboardWrite>(2);
    thread::Builder::new()
        .name("td-vm-clipboard".into())
        .spawn(move || {
            while let Ok(mut write) = receiver.recv() {
                // Payloads and endpoint diagnostics never enter the serial log.
                if conn::write_clipboard(&mut write.file, &write.bytes).is_err() {
                    eprintln!("td-compositor: VM clipboard destination closed or timed out");
                }
            }
        })
        .map_err(|e| format!("start VM clipboard writer: {e}"))?;
    runtime
        .lock()
        .map_err(|_| "runtime lock poisoned")?
        .vm_writer(Some(sender));
    let worker_runtime = Arc::clone(&runtime);
    if let Err(error) = thread::Builder::new()
        .name("td-vm-bridge".into())
        .spawn(move || serve(port, worker_runtime))
    {
        runtime
            .lock()
            .map_err(|_| "runtime lock poisoned")?
            .vm_writer(None);
        return Err(format!("start VM bridge: {error}"));
    }
    eprintln!("td-compositor: td VM bridge v1 available");
    Ok(())
}

#[derive(Default)]
struct Session {
    lease: Option<(u64, u64, Instant)>,
}

impl Session {
    fn handle(
        &mut self,
        request: wire::Message,
        runtime: &Arc<Mutex<Runtime>>,
        feed_path: &Path,
    ) -> wire::Message {
        let id = request.id;
        let result = self.dispatch(request, runtime, feed_path);
        match result {
            Ok((revision, data)) => wire::Message::new(id, wire::OK, revision, data),
            Err(error) => wire::Message::new(id, wire::ERROR, 0, error.into_bytes()),
        }
    }

    fn dispatch(
        &mut self,
        request: wire::Message,
        runtime: &Arc<Mutex<Runtime>>,
        feed_path: &Path,
    ) -> Result<(u64, Vec<u8>), String> {
        match request.verb.as_str() {
            wire::SNAPSHOT if request.data.is_empty() && request.revision == 0 => {
                self.lease = None;
                let revision = runtime
                    .lock()
                    .map_err(|_| "runtime lock poisoned")?
                    .vm_snapshot()?;
                self.lease = Some((request.id, revision, Instant::now() + wire::TIMEOUT));
                Ok((revision, b"clipboard-v1 feed-v1".to_vec()))
            }
            wire::PUT | wire::GET => {
                let (id, revision, deadline) = self
                    .lease
                    .take()
                    .ok_or("clipboard action needs a fresh snapshot")?;
                if id != request.id || revision != request.revision || Instant::now() >= deadline {
                    return Err("clipboard snapshot expired or mismatched".into());
                }
                if request.verb == wire::PUT {
                    runtime
                        .lock()
                        .map_err(|_| "runtime lock poisoned")?
                        .vm_put(revision, request.data)?;
                    Ok((revision, Vec::new()))
                } else {
                    if !request.data.is_empty() {
                        return Err("get has no payload".into());
                    }
                    Ok((revision, read_selection(runtime, revision, deadline)?))
                }
            }
            wire::KEY if request.revision == 0 => {
                self.lease = None;
                let uid = fs::metadata("/proc/self").map_err(|e| e.to_string())?.uid();
                guest_key(
                    &feed_path.with_file_name("vm-git-identity"),
                    Path::new(wire::git_key::RESPONSE),
                    &request.data, uid, 1000,
                ).map(|data| (0, data))
            }
            wire::WORKSPACE if request.revision == 0 => {
                self.lease = None;
                let uid = fs::metadata("/proc/self").map_err(|e| e.to_string())?.uid();
                guest_workspace(&feed_path.with_file_name("vm-workspace"),
                    Path::new(wire::workspace::RESPONSE), &request.data, uid, 1000)
                    .map(|data| (0, data))
            }
            wire::POWEROFF if request.revision == 0 && request.data.is_empty() => {
                self.lease = None;
                publish_request(&feed_path.with_file_name("vm-poweroff"), wire::POWER_RECORD, false)?;
                Ok((0, wire::POWER_QUEUED.to_vec()))
            }
            wire::FEED if request.revision == 0 => {
                self.lease = None;
                publish_feed(feed_path, &request.data)?;
                Ok((0, Vec::new()))
            }
            _ => Err("invalid host request".into()),
        }
    }
}

fn serve(mut port: File, runtime: Arc<Mutex<Runtime>>) {
    let mut decoder = wire::Decoder::default();
    let mut session = Session::default();
    let mut buffer = [0; 4096];
    let mut idle = 0u8;
    loop {
        match port.read(&mut buffer) {
            Ok(0) => {
                session.lease = None;
                decoder = wire::Decoder::default();
                idle = idle.saturating_add(1);
                thread::sleep(Duration::from_millis(if idle < 5 { 20 } else { 100 }));
            }
            Ok(n) => {
                idle = 0;
                let Some(bytes) = buffer.get(..n) else {
                    session.lease = None;
                    continue;
                };
                for byte in bytes {
                    if let Some(result) = decoder.push(*byte, Instant::now()) {
                        let Ok(request) = result else {
                            session.lease = None;
                            continue;
                        };
                        let response =
                            session.handle(request, &runtime, Path::new(wire::FEED_FILE));
                        let Ok(bytes) = response.encode() else {
                            session.lease = None;
                            continue;
                        };
                        if wire::write_all(&mut port, &bytes, Instant::now() + wire::TIMEOUT)
                            .is_err()
                        {
                            session.lease = None;
                        }
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                idle = idle.saturating_add(1);
                thread::sleep(Duration::from_millis(if idle < 5 { 5 } else { 100 }))
            }
            Err(_) => {
                // Hot-unplug is not reassignment. A later deployment restart
                // must obtain a fresh seatd-checked device.
                eprintln!("td-compositor: VM bridge device disconnected");
                return;
            }
        }
    }
}

fn read_selection(
    runtime: &Arc<Mutex<Runtime>>,
    revision: u64,
    deadline: Instant,
) -> Result<Vec<u8>, String> {
    let (mut reader, writer) = UnixStream::pair().map_err(|e| e.to_string())?;
    reader.set_nonblocking(true).map_err(|e| e.to_string())?;
    let file = File::from(OwnedFd::from(writer));
    let cached = runtime
        .lock()
        .map_err(|_| "runtime lock poisoned")?
        .vm_get(revision, TransferEndpoint::from_file(file))?;
    let mut data = Vec::new();
    if let Some(bytes) = cached {
        data.extend_from_slice(&bytes);
    } else {
        let mut buffer = [0; 4096];
        loop {
            runtime
                .lock()
                .map_err(|_| "runtime lock poisoned")?
                .vm_check(revision)?;
            if Instant::now() >= deadline {
                return Err("guest clipboard source timed out".into());
            }
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    if data.len() + n > wire::MAX_TEXT {
                        return Err("guest clipboard exceeds 64 KiB".into());
                    }
                    data.extend_from_slice(buffer.get(..n).ok_or("clipboard read length")?);
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5))
                }
                Err(e) => return Err(format!("guest clipboard read: {e}")),
            }
        }
    }
    runtime
        .lock()
        .map_err(|_| "runtime lock poisoned")?
        .vm_check(revision)?;
    wire::text(&data)?;
    if data.is_empty() {
        return Err("guest clipboard source returned no text".into());
    }
    Ok(data)
}

fn publish_feed(path: &Path, bytes: &[u8]) -> Result<(), String> {
    wire::feed(bytes)?;
    if bytes.is_empty() {
        return match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("clear VM feed configuration: {e}")),
        };
    }
    publish_public(path, bytes)
}

fn publish_public(path: &Path, bytes: &[u8]) -> Result<(), String> {
    publish_request(path, bytes, true)
}

fn publish_request(path: &Path, bytes: &[u8], reuse: bool) -> Result<(), String> {
    if let Ok(file) = OpenOptions::new().read(true).custom_flags(0x20000 | 0x800).open(path) {
        let mut current = Vec::new();
        let meta = file.metadata().map_err(|e| e.to_string())?;
        if !meta.is_file() { return Err("invalid VM public configuration type".into()); }
        if reuse && meta.uid() == fs::metadata("/proc/self").map_err(|e| e.to_string())?.uid()
            && meta.nlink() == 1 && meta.mode() & 0o022 == 0
            && file.take(129).read_to_end(&mut current).is_ok() && current == bytes {
            return Ok(());
        }
    }
    let mut nonce = [0; 16];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut nonce))
        .map_err(|e| format!("name VM public staging file: {e}"))?;
    let temp = path.with_extension(format!("tmp-{:032x}", u128::from_le_bytes(nonce)));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&temp)
        .map_err(|e| format!("stage VM public configuration: {e}"))?;
    let result = (|| {
        file.write_all(bytes).map_err(|e| e.to_string())?;
        file.set_permissions(fs::Permissions::from_mode(0o644))
            .map_err(|e| e.to_string())?;
        fs::rename(&temp, path).map_err(|e| format!("publish VM public configuration: {e}"))
    })();
    let _ = fs::remove_file(temp);
    result
}

fn guest_key(request: &Path, response: &Path, bytes: &[u8], compositor: u32, human: u32) -> Result<Vec<u8>, String> {
    let id = wire::git_key::identity(bytes)?;
    for (path, owner) in [(request, compositor), (response, human)] {
        let parent = path.parent().ok_or("VM key endpoint has no parent")?;
        let meta = fs::symlink_metadata(parent).map_err(|e| format!("inspect VM key runtime: {e}"))?;
        if !meta.is_dir() || meta.uid() != owner || meta.mode() & 0o022 != 0 {
            return Err("VM key runtime has an untrusted type, owner or mode".into());
        }
    }
    publish_public(request, bytes)?;
    let file = OpenOptions::new().read(true).custom_flags(0x20000 | 0x800).open(response)
        .map_err(|e| if e.kind() == std::io::ErrorKind::NotFound {
            "Guest Git key pending; retry once the guest helper has generated it".into()
        } else { format!("open guest public key: {e}") })?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.uid() != human || meta.nlink() != 1 || meta.mode() & 0o022 != 0 {
        return Err("guest public key has an untrusted type, owner or mode".into());
    }
    let mut reply = Vec::new();
    file.take((wire::git_key::LIMIT + 1) as u64).read_to_end(&mut reply).map_err(|e| e.to_string())?;
    wire::git_key::parse(&reply, id)?;
    Ok(reply)
}

fn guest_workspace(request: &Path, response: &Path, bytes: &[u8], compositor: u32, human: u32) -> Result<Vec<u8>, String> {
    let plan = wire::workspace::Plan::parse(bytes)?;
    for (path, owner) in [(request, compositor), (response, human)] {
        let meta = fs::symlink_metadata(path.parent().ok_or("workspace endpoint has no parent")?)
            .map_err(|e| e.to_string())?;
        if !meta.is_dir() || meta.uid() != owner || meta.mode() & 0o022 != 0 {
            return Err("untrusted workspace endpoint directory".into());
        }
    }
    // Replacing the request allows an explicit retry after a failed clone.
    publish_request(request, bytes, false)?;
    let file = OpenOptions::new().read(true).custom_flags(0x20000 | 0x800).open(response)
        .map_err(|e| if e.kind() == std::io::ErrorKind::NotFound {
            "Guest workspace preparation pending; retry clone to inspect completion".into()
        } else { format!("read workspace status: {e}") })?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.uid() != human || meta.nlink() != 1 || meta.mode() & 0o022 != 0 {
        return Err("untrusted workspace response".into());
    }
    let mut reply = Vec::new();
    file.take(4097).read_to_end(&mut reply).map_err(|e| e.to_string())?;
    if reply.len() > 4096 { return Err("workspace response exceeds limit".into()); }
    wire::workspace::status(&reply, &plan)?;
    Ok(reply)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]
    use super::*;
    use crate::{
        buffer::Surface,
        framebuffer::Framebuffer,
        scene::{SurfaceKey, SHM_XRGB8888},
    };
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        dir: std::path::PathBuf,
        runtime: Arc<Mutex<Runtime>>,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "td-vm-guest-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&dir).unwrap();
            let framebuffer = Framebuffer::test_file(&dir.join("fb"), 120, 100, 480).unwrap();
            let mut runtime = Runtime::new(framebuffer);
            runtime
                .commit(
                    SurfaceKey {
                        client: 1,
                        object: 1,
                    },
                    Surface::from_shm_pixels(100, 100, vec![255; 40000], SHM_XRGB8888).unwrap(),
                )
                .unwrap();
            Self {
                dir,
                runtime: Arc::new(Mutex::new(runtime)),
            }
        }
        fn request(
            &self,
            session: &mut Session,
            id: u64,
            verb: &str,
            revision: u64,
            data: &[u8],
        ) -> wire::Message {
            session.handle(
                wire::Message::new(id, verb, revision, data.to_vec()),
                &self.runtime,
                &self.dir.join("feed"),
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn poweroff_is_explicit_empty_and_replaces_only_its_request() {
        let f = Fixture::new();
        let mut session = Session::default();
        let request = f.dir.join("vm-poweroff");
        for (revision, data) in [(1, &b""[..]), (0, &b"reboot"[..])] {
            assert_eq!(f.request(&mut session, 1, wire::POWEROFF, revision, data).verb, wire::ERROR);
            assert!(!request.exists());
        }
        let reply = f.request(&mut session, 2, wire::POWEROFF, 0, b"");
        assert_eq!(reply.verb, wire::OK);
        assert_eq!(reply.data, wire::POWER_QUEUED);
        assert_eq!(fs::read(&request).unwrap(), wire::POWER_RECORD);
        let held = File::open(&request).unwrap();
        let previous = held.metadata().unwrap().ino();
        assert_eq!(f.request(&mut session, 3, wire::POWEROFF, 0, b"").verb, wire::OK);
        assert_ne!(fs::metadata(&request).unwrap().ino(), previous);
        assert!(!f.dir.join("vm-workspace").exists());
    }

    #[test]
    fn vm_clipboard_is_explicit_single_use_and_revision_bound() {
        let f = Fixture::new();
        let mut s = Session::default();
        assert_eq!(f.request(&mut s, 1, wire::PUT, 0, b"bad").verb, wire::ERROR);
        let snapshot = f.request(&mut s, 2, wire::SNAPSHOT, 0, b"");
        assert_eq!(snapshot.verb, wire::OK);
        let text = "hello\n世界\tlast".as_bytes();
        assert_eq!(
            f.request(&mut s, 2, wire::PUT, snapshot.revision, text)
                .verb,
            wire::OK
        );
        assert_eq!(
            f.request(&mut s, 2, wire::PUT, snapshot.revision, b"replay")
                .verb,
            wire::ERROR
        );
        let empty_snapshot = f.request(&mut s, 9, wire::SNAPSHOT, 0, b"");
        assert_eq!(
            f.request(&mut s, 9, wire::PUT, empty_snapshot.revision, b"")
                .verb,
            wire::ERROR
        );
        let snapshot = f.request(&mut s, 3, wire::SNAPSHOT, 0, b"");
        let result = f.request(&mut s, 3, wire::GET, snapshot.revision, b"");
        assert_eq!(result.verb, wire::OK);
        assert_eq!(result.data, text);
        let snapshot = f.request(&mut s, 4, wire::SNAPSHOT, 0, b"");
        f.runtime
            .lock()
            .unwrap()
            .vm_put(snapshot.revision, b"newer".to_vec())
            .unwrap();
        assert_eq!(
            f.request(&mut s, 4, wire::PUT, snapshot.revision, b"stale")
                .verb,
            wire::ERROR
        );
        let snapshot = f.request(&mut s, 5, wire::SNAPSHOT, 0, b"");
        s.lease = Some((
            5,
            snapshot.revision,
            Instant::now() - Duration::from_secs(1),
        ));
        assert_eq!(
            f.request(&mut s, 5, wire::GET, snapshot.revision, b"").verb,
            wire::ERROR
        );
    }

    #[test]
    fn focus_round_trip_cancels_a_snapshot_and_instances_are_independent() {
        let f = Fixture::new();
        let other = Fixture::new();
        let other_revision = {
            let mut runtime = other.runtime.lock().unwrap();
            let revision = runtime.vm_snapshot().unwrap();
            runtime.vm_put(revision, b"independent".to_vec()).unwrap();
            runtime.vm_snapshot().unwrap()
        };
        let mut session = Session::default();
        let snapshot = f.request(&mut session, 8, wire::SNAPSHOT, 0, b"");
        {
            let mut runtime = f.runtime.lock().unwrap();
            runtime
                .command(crate::layout::Command::SwitchWorkspace(2))
                .unwrap();
            assert!(runtime.vm_snapshot().is_err());
            runtime
                .command(crate::layout::Command::SwitchWorkspace(1))
                .unwrap();
        }
        assert_eq!(
            f.request(&mut session, 8, wire::PUT, snapshot.revision, b"stale")
                .verb,
            wire::ERROR
        );
        assert_eq!(
            other.runtime.lock().unwrap().vm_snapshot().unwrap(),
            other_revision
        );
        assert_eq!(
            read_selection(
                &other.runtime,
                other_revision,
                Instant::now() + wire::TIMEOUT
            )
            .unwrap(),
            b"independent"
        );
    }

    #[test]
    fn guest_source_bytes_flow_through_an_owned_endpoint_and_cancel_on_replacement() {
        use crate::runtime::{DataSourceIdentity, KeyboardDelivery, SelectionSource};
        let f = Fixture::new();
        let (receiver, _stop) = f
            .runtime
            .lock()
            .unwrap()
            .subscribe_keyboard(1)
            .unwrap()
            .split();
        let source = SelectionSource {
            identity: DataSourceIdentity {
                client: 1,
                object: 7,
                generation: 42,
            },
            mime_types: Arc::new(vec!["text/plain;charset=utf-8".into()]),
        };
        f.runtime
            .lock()
            .unwrap()
            .set_selection(1, Some(source.clone()))
            .unwrap();
        let revision = f.runtime.lock().unwrap().vm_snapshot().unwrap();
        let writer = thread::spawn(move || {
            let KeyboardDelivery::DataSourceSend {
                file, mime_type, ..
            } = receiver.recv_timeout(wire::TIMEOUT).unwrap()
            else {
                panic!("expected a source transfer");
            };
            assert_eq!(mime_type, "text/plain;charset=utf-8");
            file.into_file()
                .write_all("actual source\n世界".as_bytes())
                .unwrap();
            receiver
        });
        assert_eq!(
            read_selection(&f.runtime, revision, Instant::now() + wire::TIMEOUT).unwrap(),
            "actual source\n世界".as_bytes()
        );
        let receiver = writer.join().unwrap();
        let runtime = Arc::clone(&f.runtime);
        let empty = thread::spawn(move || {
            read_selection(&runtime, revision, Instant::now() + wire::TIMEOUT)
        });
        let KeyboardDelivery::DataSourceSend { file, .. } =
            receiver.recv_timeout(wire::TIMEOUT).unwrap()
        else {
            panic!("expected a pending transfer");
        };
        drop(file);
        assert!(empty.join().unwrap().unwrap_err().contains("no text"));
        let runtime = Arc::clone(&f.runtime);
        let reader = thread::spawn(move || {
            read_selection(&runtime, revision, Instant::now() + wire::TIMEOUT)
        });
        let KeyboardDelivery::DataSourceSend { file, .. } =
            receiver.recv_timeout(wire::TIMEOUT).unwrap()
        else {
            panic!("expected a pending transfer");
        };
        f.runtime.lock().unwrap().set_selection(1, None).unwrap();
        assert!(reader.join().unwrap().unwrap_err().contains("changed"));
        drop(file);
    }

    #[test]
    fn imported_selection_uses_the_normal_focused_receive_route() {
        let f = Fixture::new();
        let (writer, receiver) = mpsc::sync_channel(2);
        let mut runtime = f.runtime.lock().unwrap();
        runtime.vm_writer(Some(writer));
        let revision = runtime.vm_snapshot().unwrap();
        runtime.vm_put(revision, b"imported".to_vec()).unwrap();
        let source = runtime.selection_for_client(1).2.unwrap().identity;
        let (mut reader, writer) = UnixStream::pair().unwrap();
        reader.set_read_timeout(Some(wire::TIMEOUT)).unwrap();
        let file = TransferEndpoint::from_file(File::from(OwnedFd::from(writer)));
        assert!(runtime
            .send_selection_data(1, source, "text/plain".into(), file)
            .unwrap());
        drop(runtime);
        let mut write = receiver.recv_timeout(wire::TIMEOUT).unwrap();
        write.file.write_all(&write.bytes).unwrap();
        drop(write);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"imported");
    }

    #[test]
    fn provisioned_feed_is_volatile_data_with_a_fixed_authority() {
        let f = Fixture::new();
        let mut s = Session::default();
        for endpoint in [
            b"http://host:4".as_slice(),
            b"http://10.0.2.2:4/exec",
            b"http://10.0.2.2:4\n",
        ] {
            assert_eq!(
                f.request(&mut s, 1, wire::FEED, 0, endpoint).verb,
                wire::ERROR
            );
        }
        assert!(!f.dir.join("feed").exists());
        assert_eq!(
            f.request(&mut s, 2, wire::FEED, 0, b"http://10.0.2.2:1234")
                .verb,
            wire::OK
        );
        assert_eq!(
            fs::read(f.dir.join("feed")).unwrap(),
            b"http://10.0.2.2:1234"
        );
        assert_eq!(f.request(&mut s, 3, wire::FEED, 0, b"").verb, wire::OK);
        assert!(!f.dir.join("feed").exists());
    }
    #[test]
    fn git_key_exchange_publishes_only_valid_identity_and_checks_public_reply() {
        let f = Fixture::new();
        let uid = fs::metadata(&f.dir).unwrap().uid();
        let request = f.dir.join("vm-git-identity");
        let response = f.dir.join("git-key");
        let id = "0123456789abcdef0123456789abcdef";
        let key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEB";
        assert!(guest_key(&request, &response, b"../path", uid, uid).is_err());
        assert!(!request.exists());
        assert!(guest_key(&request, &response, id.as_bytes(), uid, uid).unwrap_err().contains("pending"));
        assert_eq!(fs::read(&request).unwrap(), id.as_bytes());
        fs::set_permissions(&request, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(guest_key(&request, &response, id.as_bytes(), uid, uid).unwrap_err().contains("pending"));
        assert_eq!(fs::metadata(&request).unwrap().mode() & 0o777, 0o644);
        let alias = f.dir.join("request-alias");
        fs::hard_link(&request, &alias).unwrap();
        assert!(guest_key(&request, &response, id.as_bytes(), uid, uid).unwrap_err().contains("pending"));
        assert_eq!(fs::metadata(&request).unwrap().nlink(), 1);
        assert_ne!(fs::metadata(&request).unwrap().ino(), fs::metadata(&alias).unwrap().ino());
        let reply = wire::git_key::encode(id, key).unwrap();
        fs::write(&response, &reply).unwrap();
        fs::set_permissions(&response, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(guest_key(&request, &response, id.as_bytes(), uid, uid).unwrap(), reply);
        assert!(guest_key(&request, &response, b"1123456789abcdef0123456789abcdef", uid, uid).is_err());
        fs::set_permissions(&response, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(guest_key(&request, &response, id.as_bytes(), uid, uid).is_err());
        fs::set_permissions(&response, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(guest_key(&request, &response, id.as_bytes(), uid, uid + 1).is_err());
        fs::write(&response, [reply, b"PRIVATE DATA".to_vec()].concat()).unwrap();
        assert!(guest_key(&request, &response, id.as_bytes(), uid, uid).is_err());
        fs::remove_file(&response).unwrap();
        std::os::unix::fs::symlink(&request, &response).unwrap();
        assert!(guest_key(&request, &response, id.as_bytes(), uid, uid).is_err());
        assert_eq!(fs::read(&request).unwrap(), id.as_bytes());
    }

    #[test]
    fn workspace_exchange_retries_and_accepts_only_matching_owned_completion() {
        let f = Fixture::new();
        let uid = fs::metadata("/proc/self").unwrap().uid();
        let request = f.dir.join("vm-workspace"); let response = f.dir.join("workspace-reply");
        let plan = wire::workspace::example(); let encoded = plan.encode();
        assert!(guest_workspace(&request, &response, b"invalid", uid, uid).is_err());
        assert!(!request.exists());
        assert!(guest_workspace(&request, &response, &encoded, uid, uid).unwrap_err().contains("pending"));
        let first = fs::metadata(&request).unwrap();
        fs::write(&response, wire::workspace::ready(&plan)).unwrap();
        assert!(guest_workspace(&request, &response, &encoded, uid, uid).is_ok());
        assert_ne!(first.ino(), fs::metadata(&request).unwrap().ino());
        let mut other = plan.clone(); other.commit = "b".repeat(40);
        fs::write(&response, wire::workspace::ready(&other)).unwrap();
        assert!(guest_workspace(&request, &response, &encoded, uid, uid).is_err());
        fs::write(&response, wire::workspace::failure(&plan, "Git clone failed")).unwrap();
        assert!(guest_workspace(&request, &response, &encoded, uid, uid).unwrap_err().contains("Git clone failed"));
        fs::set_permissions(&response, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(guest_workspace(&request, &response, &encoded, uid, uid).unwrap_err().contains("untrusted"));
        assert!(guest_workspace(&request, &response, &encoded, uid + 1, uid).is_err());
    }

}
