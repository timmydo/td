//! Private local control for the foreground receiving profile.
use super::{config_failure, lock_root, ConfigFailure};
use std::{
    fs,
    io::{self, Read, Write},
    os::unix::{
        fs::{FileTypeExt, MetadataExt},
        net::UnixStream,
    },
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use td_mta::{
    smtp_receiving::{Control, State},
    store_fs::PrivateRoot,
};

const NAME: &str = "smtp-control.sock";
const TURN: Duration = Duration::from_millis(100);
const EXCHANGE: Duration = Duration::from_secs(2);

pub(super) struct Endpoint {
    listener: td_mta::unix_listener::PrivateListener,
}
impl Endpoint {
    pub(super) fn bind(root: PrivateRoot, path: &str) -> Result<Self, ConfigFailure> {
        check_runtime_path(path)?;
        let root = lock_root(root, "control-lock")?;
        let path = td_fs::SocketPath::new(&PathBuf::from(path).join(NAME)).map_err(io_failure)?;
        let listener = td_mta::unix_listener::PrivateListener::bind_locked(path, root)
            .map_err(|_| config_failure("control", "socket-policy"))?;
        Ok(Self { listener })
    }

    pub(super) fn run(
        &self,
        control: &Control,
        finished: &AtomicBool,
        mailbox_access: bool,
        queue: Option<&td_mta::postgres_relay_worker::QueueControl>,
    ) -> io::Result<()> {
        let mut retry_at = Instant::now();
        while !finished.load(Ordering::Acquire) {
            if super::signal_sys::requested()? {
                control.user_stop();
            }
            if Instant::now() < retry_at {
                std::thread::sleep(TURN);
                continue;
            }
            match self.listener.accept() {
                Ok(mut stream) => {
                    // One private operator request; neither storage nor SMTP waits on it.
                    let _ = exchange_queue(&mut stream, control, mailbox_access, queue);
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
#[cfg(test)]
fn exchange(stream: &mut UnixStream, control: &Control, mailbox_access: bool) -> io::Result<()> {
    exchange_queue(stream, control, mailbox_access, None)
}
fn exchange_queue(
    stream: &mut UnixStream,
    control: &Control,
    mailbox_access: bool,
    queue: Option<&td_mta::postgres_relay_worker::QueueControl>,
) -> io::Result<()> {
    let start = Instant::now();
    let mut bytes = [0; 256];
    let request = line(stream, start, &mut bytes, true)?;
    if request == b"PERFORMANCE-STATUS\n" {
        let Some(logger) = control.logging() else {
            return send(
                stream,
                start,
                b"{\"schema\":1,\"command\":\"performance-status\",\"status\":\"unavailable\"}\n",
            );
        };
        let mut bytes = [0; td_mta::performance::MAX_STATUS_BYTES];
        let mut output = td_mta::bounded::TextBuffer::new(&mut bytes);
        logger
            .performance_status(&mut output)
            .map_err(|_| io::ErrorKind::InvalidData)?;
        return send(
            stream,
            start,
            output.as_bytes().map_err(|_| io::ErrorKind::InvalidData)?,
        );
    }
    if request == b"LOG-STATUS\n" {
        let value = control.logging_status();
        let response=match value {Some(status)=>format!("{{\"schema\":1,\"command\":\"logging-status\",\"status\":\"ok\",\"dropped\":{},\"write_failures\":{},\"degraded\":{}}}\n",status.dropped,status.write_failures,status.degraded),None=>"{\"schema\":1,\"command\":\"logging-status\",\"status\":\"unavailable\"}\n".to_owned()};
        return send(stream, start, response.as_bytes());
    }
    let response: &[u8] = match request {
        b"STATUS\n" => match control.state() {
            State::Starting => b"STARTING\n",
            State::Ready => b"READY\n",
            State::Stopping => b"STOPPING\n",
            State::Stopped => b"STOPPED\n",
            State::Failed => b"FAILED\n",
        },
        b"ROUTE-REPAIR\n" if queue.is_some() => {
            if let Some(queue) = queue {
                queue.request_repair();
            }
            b"REQUESTED\n"
        }
        b"ROUTE-STATUS\n" if queue.is_some() => match queue.map(|q| q.repair_status()) {
            Some("requested") => b"REQUESTED\n",
            Some("repaired") => b"REPAIRED\n",
            Some("refused") => b"REFUSED\n",
            _ => b"IDLE\n",
        },
        b"CLOCK-CONFIRM\n" if queue.is_some() => {
            if let Some(queue) = queue {
                queue.request_confirmation();
            }
            b"REQUESTED\n"
        }
        b"CLOCK-STATUS\n" if queue.is_some() => {
            if queue.is_some_and(|queue| queue.expiry_enabled()) {
                b"CONFIRMED\n"
            } else {
                b"HELD\n"
            }
        }
        b"STOP\n" => {
            control.user_stop();
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

pub(super) fn clock_request(runtime: &str, command: &str) -> Result<&'static str, ConfigFailure> {
    let (_root, mut stream) = connect(runtime)?;
    let start = Instant::now();
    send(
        &mut stream,
        start,
        match command {
            "queue-clock-confirm" => b"CLOCK-CONFIRM\n",
            "queue-route-repair" => b"ROUTE-REPAIR\n",
            "queue-route-status" => b"ROUTE-STATUS\n",
            _ => b"CLOCK-STATUS\n",
        },
    )
    .map_err(io_failure)?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(io_failure)?;
    let mut bytes = [0; 32];
    let reply = line(&mut stream, start, &mut bytes, false).map_err(io_failure)?;
    match reply.strip_prefix(b"HTTPS ").unwrap_or(reply) {
        b"REQUESTED\n" => Ok("requested"),
        b"CONFIRMED\n" => Ok("confirmed"),
        b"HELD\n" => Ok("held"),
        b"REPAIRED\n" => Ok("repaired"),
        b"REFUSED\n" => Ok("refused"),
        b"IDLE\n" => Ok("idle"),
        _ => Err(config_failure("queue", "unavailable")),
    }
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
pub(super) fn logging_request(runtime: &str) -> Result<String, ConfigFailure> {
    let (_root, mut stream) = connect(runtime)?;
    let start = Instant::now();
    send(&mut stream, start, b"LOG-STATUS\n").map_err(io_failure)?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(io_failure)?;
    let mut bytes = [0; 256];
    let response = line(&mut stream, start, &mut bytes, false).map_err(io_failure)?;
    let text = std::str::from_utf8(response)
        .map_err(|_| super::config_failure("logging", "invalid-response"))?;
    let json =
        td_json::parse(text).map_err(|_| super::config_failure("logging", "invalid-response"))?;
    Ok(json.to_string())
}

pub(super) fn performance_request(runtime: &str) -> Result<String, ConfigFailure> {
    let (_root, mut stream) = connect(runtime)?;
    let start = Instant::now();
    send(&mut stream, start, b"PERFORMANCE-STATUS\n").map_err(io_failure)?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(io_failure)?;
    let mut bytes = [0; td_mta::performance::MAX_STATUS_BYTES];
    let response = line(&mut stream, start, &mut bytes, false).map_err(io_failure)?;
    let text =
        std::str::from_utf8(response).map_err(|_| config_failure("control", "invalid-response"))?;
    let json = td_json::parse(text).map_err(|_| config_failure("control", "invalid-response"))?;
    if json.get("schema").and_then(td_json::Json::as_u64) != Some(1)
        || json.get("command").and_then(td_json::Json::as_str) != Some("performance-status")
    {
        return Err(config_failure("control", "invalid-response"));
    }
    Ok(json.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn private_performance_snapshot_is_bounded_and_does_not_change_service_state() {
        use std::sync::Arc;
        let (producer, _writer) = td_mta::application_log::Producer::new(
            Arc::new(td_mta::clock::RuntimeClock::new()),
            7,
            td_mta::observability::Severity::Info,
        )
        .unwrap();
        let control = Control::with_logging(producer);
        let (mut client, mut server) = UnixStream::pair().unwrap();
        client.write_all(b"PERFORMANCE-STATUS\n").unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        exchange(&mut server, &control, true).unwrap();
        let mut bytes = [0; td_mta::performance::MAX_STATUS_BYTES];
        let response = line(&mut client, Instant::now(), &mut bytes, false).unwrap();
        let json = td_json::parse(std::str::from_utf8(response).unwrap()).unwrap();
        assert_eq!(
            json.get("command").and_then(td_json::Json::as_str),
            Some("performance-status")
        );
        assert_eq!(
            json.get("config_generation")
                .and_then(td_json::Json::as_u64),
            Some(7)
        );
        assert!(json
            .get("phases")
            .and_then(|v| v.get("http_download"))
            .is_some());
        assert_eq!(control.state(), State::Starting);
    }

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
            exchange(&mut server, &control, false).unwrap();
            let mut bytes = [0; 16];
            assert_eq!(
                line(&mut client, Instant::now(), &mut bytes, false).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn private_protocol_refuses_oversized_input() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        client.write_all(&[b'x'; 256]).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        assert!(exchange(&mut server, &Control::default(), false).is_err());
    }

    #[test]
    fn stop_waits_for_request_eof_and_refuses_fragmented_suffix() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let control = Control::default();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| exchange(&mut server, &control, false));
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
