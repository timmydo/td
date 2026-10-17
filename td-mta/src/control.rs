//! Private local control for the foreground receiving profile.
use super::{config_failure, lock_root, ConfigFailure};
use std::{
    fs,
    io::{self, Read, Write},
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};
use td_mta::{
    ports::Clock,
    smtp_receiving::{Control, State},
    store_fs::{DeviceStore, LockedRoot, PrivateRoot},
};

const NAME: &str = "smtp-control.sock";
const TURN: Duration = Duration::from_millis(100);
const EXCHANGE: Duration = Duration::from_secs(2);
const DEVICE_WAIT: Duration = Duration::from_secs(30);

pub(super) struct Endpoint {
    listener: UnixListener,
    path: PathBuf,
    device: u64,
    inode: u64,
    _root: LockedRoot,
}
impl Endpoint {
    pub(super) fn bind(root: PrivateRoot, path: &str) -> Result<Self, ConfigFailure> {
        check_runtime_path(path)?;
        let root = lock_root(root, "control-lock")?;
        let path = PathBuf::from(path).join(NAME);
        let owner = root
            .root()
            .directory()
            .metadata()
            .map_err(io_failure)?
            .uid();
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_socket() && metadata.uid() == owner => {
                fs::remove_file(&path).map_err(io_failure)?;
            }
            Ok(_) => return Err(config_failure("control", "socket-policy")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(io_failure(error)),
        }
        let listener = UnixListener::bind(&path).map_err(io_failure)?;
        let metadata = fs::symlink_metadata(&path).map_err(io_failure)?;
        let endpoint = Self {
            listener,
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
            _root: root,
        };
        fs::set_permissions(&endpoint.path, fs::Permissions::from_mode(0o600))
            .map_err(io_failure)?;
        endpoint
            .listener
            .set_nonblocking(true)
            .map_err(io_failure)?;
        let metadata = fs::symlink_metadata(&endpoint.path).map_err(io_failure)?;
        if !metadata.file_type().is_socket()
            || metadata.uid() != owner
            || metadata.mode() & 0o7777 != 0o600
            || metadata.dev() != endpoint.device
            || metadata.ino() != endpoint.inode
        {
            return Err(config_failure("control", "socket-policy"));
        }
        Ok(endpoint)
    }

    pub(super) fn run(
        &self,
        control: &Control,
        finished: &AtomicBool,
        mailbox_access: bool,
        devices: Option<&Devices<'_, '_>>,
    ) -> io::Result<()> {
        let mut retry_at = Instant::now();
        while !finished.load(Ordering::Acquire) {
            if super::signal_sys::requested()? {
                control.stop();
            }
            if Instant::now() < retry_at {
                std::thread::sleep(TURN);
                continue;
            }
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    // One private operator request; neither storage nor SMTP waits on it.
                    let _ = exchange(&mut stream, control, devices, mailbox_access);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(TURN);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => (),
                Err(error) if error.kind() == io::ErrorKind::ConnectionAborted => {
                    std::thread::sleep(TURN);
                }
                Err(error) if matches!(error.raw_os_error(), Some(12 | 23 | 24 | 105)) => {
                    // Local descriptor/memory pressure must not restart mail storage.
                    retry_at = Instant::now()
                        .checked_add(Duration::from_secs(5))
                        .ok_or(io::ErrorKind::InvalidInput)?;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
            metadata.file_type().is_socket()
                && metadata.dev() == self.device
                && metadata.ino() == self.inode
        }) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(super) fn check_runtime_path(path: &str) -> Result<(), ConfigFailure> {
    if path
        .len()
        .checked_add(NAME.len() + 1)
        .is_none_or(|length| length > 107)
    {
        return Err(config_failure("control", "socket-path-too-long"));
    }
    Ok(())
}
fn io_failure(error: io::Error) -> ConfigFailure {
    config_failure(
        "control",
        match error.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => "not-running",
            _ => "io",
        },
    )
}
fn remaining(start: Instant) -> io::Result<Duration> {
    remaining_with_limit(start, EXCHANGE)
}
fn remaining_with_limit(start: Instant, limit: Duration) -> io::Result<Duration> {
    limit
        .checked_sub(start.elapsed())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| io::ErrorKind::TimedOut.into())
}
fn line<'a>(
    stream: &mut UnixStream,
    start: Instant,
    bytes: &'a mut [u8],
    eof: bool,
) -> io::Result<&'a [u8]> {
    line_with_limit(stream, start, bytes, eof, EXCHANGE)
}
fn line_with_limit<'a>(
    stream: &mut UnixStream,
    start: Instant,
    bytes: &'a mut [u8],
    eof: bool,
    limit: Duration,
) -> io::Result<&'a [u8]> {
    let mut used = 0;
    loop {
        stream.set_read_timeout(Some(remaining_with_limit(start, limit)?))?;
        let tail = bytes.get_mut(used..).ok_or(io::ErrorKind::InvalidData)?;
        if tail.is_empty() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        match stream.read(tail) {
            Ok(0) if eof => break,
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => used += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
        let complete = bytes.get(..used).ok_or(io::ErrorKind::InvalidData)?;
        if !eof && complete.contains(&b'\n') {
            break;
        }
    }
    bytes
        .get(..used)
        .ok_or_else(|| io::ErrorKind::InvalidData.into())
}
fn send(stream: &mut UnixStream, start: Instant, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        stream.set_write_timeout(Some(remaining(start)?))?;
        match stream.write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => bytes = bytes.get(n..).ok_or(io::ErrorKind::InvalidData)?,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => (),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}
fn exchange(
    stream: &mut UnixStream,
    control: &Control,
    devices: Option<&Devices<'_, '_>>,
    mailbox_access: bool,
) -> io::Result<()> {
    let start = Instant::now();
    let mut bytes = [0; 256];
    let request = line(stream, start, &mut bytes, true)?;
    if request.starts_with(b"DEVICE ") {
        return device_exchange(stream, request, devices, control);
    }
    let response: &[u8] = match request {
        b"STATUS\n" => match control.state() {
            State::Starting => b"STARTING\n",
            State::Ready => b"READY\n",
            State::Stopping => b"STOPPING\n",
            State::Stopped => b"STOPPED\n",
            State::Failed => b"FAILED\n",
        },
        b"STOP\n" => {
            control.stop();
            b"REQUESTED\n"
        }
        _ => b"INVALID\n",
    };
    if mailbox_access {
        send(stream, start, b"HTTPS ")?;
    }
    send(stream, start, response)
}

pub(super) fn request(
    runtime: &str,
    stop: bool,
) -> Result<(&'static str, &'static str), ConfigFailure> {
    let (_root, mut stream) = connect(runtime)?;
    let start = Instant::now();
    send(
        &mut stream,
        start,
        if stop { b"STOP\n" } else { b"STATUS\n" },
    )
    .map_err(io_failure)?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(io_failure)?;
    let mut bytes = [0; 16];
    let response = line(&mut stream, start, &mut bytes, false).map_err(io_failure)?;
    let (response, profile) = match response.strip_prefix(b"HTTPS ") {
        Some(response) => (response, "mailbox-access"),
        None => (response, "smtp-only"),
    };
    let state = match (stop, response) {
        (true, b"REQUESTED\n") => Ok("requested"),
        (false, b"STARTING\n") => Ok("starting"),
        (false, b"READY\n") => Ok("ready"),
        (false, b"STOPPING\n") => Ok("stopping"),
        (false, b"STOPPED\n") => Ok("stopped"),
        (false, b"FAILED\n") => Ok("failed"),
        _ => Err(config_failure("control", "invalid-response")),
    }?;
    Ok((state, profile))
}

fn connect(runtime: &str) -> Result<(PrivateRoot, UnixStream), ConfigFailure> {
    check_runtime_path(runtime)?;
    let root =
        PrivateRoot::open(runtime).map_err(|_| config_failure("control", "root-policy-or-io"))?;
    let path = PathBuf::from(runtime).join(NAME);
    let metadata = fs::symlink_metadata(&path).map_err(io_failure)?;
    let owner = root.directory().metadata().map_err(io_failure)?.uid();
    if !metadata.file_type().is_socket()
        || metadata.uid() != owner
        || metadata.mode() & 0o7777 != 0o600
    {
        return Err(config_failure("control", "socket-policy"));
    }
    let stream = UnixStream::connect(&path).map_err(io_failure)?;
    Ok((root, stream))
}
const DEVICE_REPLY_BYTES: usize = 12 * 1024;
struct DeviceReply(Vec<u8>);
impl DeviceReply {
    fn failure(&mut self, command: &str, error: super::Failure) -> io::Result<()> {
        self.0.fill(0);
        std::hint::black_box(&mut self.0);
        self.0.clear();
        super::failure(&mut self.0, command, error)
    }
}
impl Drop for DeviceReply {
    fn drop(&mut self) {
        self.0.fill(0);
        std::hint::black_box(&mut self.0);
    }
}
pub(super) struct Devices<'a, 'r> {
    pub store: &'a Mutex<DeviceStore<'r>>,
    pub clock: &'a dyn Clock,
}
fn device_exchange(
    stream: &mut UnixStream,
    request: &[u8],
    devices: Option<&Devices<'_, '_>>,
    control: &Control,
) -> io::Result<()> {
    let operation = super::device::Operation::parse(request);
    let command = operation
        .as_ref()
        .map_or("device", |operation| operation.name());
    let mut reply = DeviceReply(Vec::with_capacity(DEVICE_REPLY_BYTES));
    let result = (|| {
        let operation = operation.as_ref().ok_or(super::Failure {
            stage: "arguments",
            code: "invalid-arguments",
        })?;
        let devices = devices.ok_or(super::Failure {
            stage: "devices",
            code: "inactive",
        })?;
        let mut store = devices.store.lock().map_err(|_| super::Failure {
            stage: "devices",
            code: "unavailable",
        })?;
        let now = devices
            .clock
            .sample()
            .map_err(|error| super::adapter("clock", error))?
            .utc_ms;
        let now = u64::try_from(now).map_err(|_| super::Failure {
            stage: "clock",
            code: "invalid",
        })?;
        let result = super::device::apply(&mut store, operation, now, &mut reply.0);
        if store.device_ids().is_err() {
            control.fail();
        }
        result
    })();
    if let Err(error) = result {
        reply.failure(command, error)?;
    }
    if reply.0.len() > DEVICE_REPLY_BYTES {
        return Err(io::ErrorKind::InvalidData.into());
    }
    send(stream, Instant::now(), &reply.0)
}
pub(super) fn device_request(
    runtime: &str,
    operation: &super::device::Operation,
    output: &mut impl Write,
) -> Result<bool, ConfigFailure> {
    let request = operation
        .wire()
        .ok_or_else(|| config_failure("arguments", "invalid-arguments"))?;
    let (_root, mut stream) = connect(runtime)?;
    let start = Instant::now();
    send(&mut stream, start, request.as_bytes()).map_err(io_failure)?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(io_failure)?;
    let mut reply = DeviceReply(vec![0; DEVICE_REPLY_BYTES + 1]);
    let reply = line_with_limit(&mut stream, Instant::now(), &mut reply.0, true, DEVICE_WAIT)
        .map_err(io_failure)?;
    if reply.len() > DEVICE_REPLY_BYTES {
        return Err(config_failure("control", "invalid-response"));
    }
    let ok = format!(
        "{{\"schema\":1,\"command\":\"{}\",\"status\":\"ok\",",
        operation.name()
    );
    let failed = format!(
        "{{\"schema\":1,\"command\":\"{}\",\"status\":\"error\",",
        operation.name()
    );
    let success = if reply.starts_with(ok.as_bytes()) {
        true
    } else if reply.starts_with(failed.as_bytes()) {
        false
    } else {
        return Err(config_failure("control", "invalid-response"));
    };
    if !reply.ends_with(b"}\n") || reply.iter().filter(|byte| **byte == b'\n').count() != 1 {
        return Err(config_failure("control", "invalid-response"));
    }
    output.write_all(reply).map_err(io_failure)?;
    output.flush().map_err(io_failure)?;
    Ok(success)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn runtime_path_reserves_the_linux_socket_name_and_terminator() {
        assert!(check_runtime_path(&format!("/{}", "r".repeat(88))).is_ok());
        assert!(check_runtime_path(&format!("/{}", "r".repeat(89))).is_err());
    }

    #[test]
    fn private_protocol_reports_starting_and_acknowledges_named_stop() {
        let control = Control::default();
        for (request, expected) in [
            (b"STATUS\n".as_slice(), b"STARTING\n".as_slice()),
            (b"STOP extra\n".as_slice(), b"INVALID\n".as_slice()),
            (b"STOP\nextra".as_slice(), b"INVALID\n".as_slice()),
            (b"STATUS".as_slice(), b"INVALID\n".as_slice()),
            (b"STOP\n".as_slice(), b"REQUESTED\n".as_slice()),
        ] {
            let (mut client, mut server) = UnixStream::pair().unwrap();
            client.write_all(request).unwrap();
            client.shutdown(std::net::Shutdown::Write).unwrap();
            exchange(&mut server, &control, None, false).unwrap();
            let mut bytes = [0; 16];
            assert_eq!(
                line(&mut client, Instant::now(), &mut bytes, false).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn device_error_replaces_provisional_success() {
        let mut reply =
            DeviceReply(b"{\"schema\":1,\"status\":\"ok\",\"secret\":\"partial".to_vec());
        reply
            .failure(
                "device.list",
                super::super::Failure {
                    stage: "devices",
                    code: "unavailable",
                },
            )
            .unwrap();
        let value = td_json::parse_slice(&reply.0).unwrap();
        assert_eq!(
            value.get("status").and_then(td_json::Json::as_str),
            Some("error")
        );
        assert!(!reply.0.windows(7).any(|bytes| bytes == b"partial"));
    }

    #[test]
    fn private_protocol_refuses_oversized_input() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        client.write_all(&[b'x'; 256]).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        assert!(exchange(&mut server, &Control::default(), None, false).is_err());
    }

    #[test]
    fn stop_waits_for_request_eof_and_refuses_fragmented_suffix() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let control = Control::default();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| exchange(&mut server, &control, None, false));
            client.write_all(b"STOP\n").unwrap();
            client
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            let mut one = [0];
            let error = client.read(&mut one).unwrap_err();
            assert!(matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ));
            client.write_all(b"extra").unwrap();
            client.shutdown(std::net::Shutdown::Write).unwrap();
            let mut bytes = [0; 16];
            assert_eq!(
                line(&mut client, Instant::now(), &mut bytes, false).unwrap(),
                b"INVALID\n"
            );
            worker.join().unwrap().unwrap();
        });
    }
}
