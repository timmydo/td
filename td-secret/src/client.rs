//! Bounded credential client; the broker supplies identity, never the environment.

use crate::{message, name, store, sys, wire};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::RawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const BUS: &str = "org.freedesktop.DBus";
const PORTAL: &str = "org.freedesktop.portal.Desktop";
const MAX_FRAME: usize = 16 * 1024;

#[derive(Default)]
struct Descriptors(Vec<RawFd>);
impl Drop for Descriptors {
    fn drop(&mut self) {
        sys::discard_received(&self.0);
    }
}

struct Frame {
    bytes: Vec<u8>,
    fds: Descriptors,
}

struct Client {
    stream: UnixStream,
    deadline: Instant,
    serial: u32,
}
impl Client {
    fn remaining(&self) -> Result<Duration, String> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| "credential portal deadline expired".into())
    }
    fn line(&mut self) -> Result<String, String> {
        let mut line = Vec::new();
        while line.len() < 512 {
            self.stream
                .set_read_timeout(Some(self.remaining()?))
                .map_err(|e| e.to_string())?;
            let mut byte = [0u8; 1];
            self.stream
                .read_exact(&mut byte)
                .map_err(|e| e.to_string())?;
            if byte == *b"\n" {
                return String::from_utf8(line).map_err(|e| e.to_string());
            }
            line.extend_from_slice(&byte);
        }
        Err("oversized bus authentication line".into())
    }
    fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.stream
            .set_write_timeout(Some(self.remaining()?))
            .map_err(|e| e.to_string())?;
        self.stream.write_all(bytes).map_err(|e| e.to_string())
    }
    fn read_part(&self, mut bytes: &mut [u8], fds: &mut Descriptors) -> Result<(), String> {
        while !bytes.is_empty() {
            self.stream
                .set_read_timeout(Some(self.remaining()?))
                .map_err(|e| e.to_string())?;
            let received = sys::recv_with_fds(&self.stream, bytes).map_err(|e| e.to_string())?;
            fds.0.extend(received.fds);
            if fds.0.len() > 1 {
                return Err("too many credential descriptors".into());
            }
            if received.count == 0 {
                return Err("credential bus disconnected".into());
            }
            bytes = bytes
                .get_mut(received.count..)
                .ok_or("invalid credential receive length")?;
        }
        Ok(())
    }
    fn frame(&self) -> Result<Frame, String> {
        let mut head = [0u8; 16];
        let mut fds = Descriptors::default();
        self.read_part(&mut head, &mut fds)?;
        let total = message::frame_len(&head)
            .map_err(|e| e.to_string())?
            .ok_or("short bus header")?;
        if !(16..=MAX_FRAME).contains(&total) {
            return Err("oversized credential bus frame".into());
        }
        let mut bytes = head.to_vec();
        bytes.resize(total, 0);
        self.read_part(
            bytes.get_mut(16..).ok_or("missing bus frame tail")?,
            &mut fds,
        )?;
        message::decode(&bytes, fds.0.len() as u32).map_err(|e| e.to_string())?;
        Ok(Frame { bytes, fds })
    }
    fn call(
        &mut self,
        destination: &str,
        path: &str,
        interface: &str,
        member: &str,
        argument: Option<&str>,
        sender: Option<&str>,
    ) -> Result<Frame, String> {
        self.serial = self
            .serial
            .checked_add(1)
            .ok_or("credential bus serial exhausted")?;
        let mut builder =
            message::Builder::method_call(wire::Endian::Little, path, Some(interface), member)
                .destination(destination)
                .serial(self.serial);
        if let Some(argument) = argument {
            builder = builder
                .body("s", |w| w.string(argument))
                .map_err(|e| e.to_string())?;
        }
        self.write(&builder.encode().map_err(|e| e.to_string())?)?;
        for _ in 0..32 {
            let frame = self.frame()?;
            let (reply, _) = message::decode(&frame.bytes, frame.fds.0.len() as u32)
                .map_err(|e| e.to_string())?;
            if reply.fields.reply_serial != Some(self.serial) {
                continue;
            }
            if sender.is_some() && reply.fields.sender != sender {
                return Err("credential reply has wrong sender".into());
            }
            if reply.kind == message::MessageType::Error {
                return Err(format!(
                    "credential portal refused the request: {}",
                    reply
                        .fields
                        .error_name
                        .unwrap_or("unspecified remote error")
                ));
            }
            if reply.kind != message::MessageType::MethodReturn {
                return Err("invalid credential reply type".into());
            }
            return Ok(frame);
        }
        Err("too much unrelated credential bus traffic".into())
    }
}

fn one_string(frame: &Frame) -> Result<String, String> {
    let (reply, _) =
        message::decode(&frame.bytes, frame.fds.0.len() as u32).map_err(|e| e.to_string())?;
    match reply.args() {
        [wire::Value::Str(value)]
            if reply.fields.signature == Some("s") && frame.fds.0.is_empty() =>
        {
            Ok((*value).into())
        }
        _ => Err("invalid credential bus string reply".into()),
    }
}

pub fn retrieve(name: &str) -> Result<Vec<u8>, String> {
    if !store::valid_name(name) {
        return Err("invalid credential name".into());
    }
    let address = std::env::var("DBUS_SESSION_BUS_ADDRESS")
        .map_err(|_| "no session bus in this application")?;
    let path = address
        .strip_prefix("unix:path=")
        .filter(|p| p.starts_with('/') && !p.contains([';', ',', '%', '\0']))
        .ok_or("credential client needs one absolute unix:path bus")?;
    retrieve_at(PathBuf::from(path), name)
}

fn retrieve_at(path: PathBuf, name: &str) -> Result<Vec<u8>, String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = tx.send(UnixStream::connect(path));
    });
    let stream = rx
        .recv_timeout(Duration::from_secs(20))
        .map_err(|_| "credential bus connect timed out")?
        .map_err(|e| e.to_string())?;
    let mut client = Client {
        stream,
        deadline,
        serial: 0,
    };
    let uid = fs::metadata("/proc/self").map_err(|e| e.to_string())?.uid();
    let identity = uid
        .to_string()
        .bytes()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    client.write(format!("\0AUTH EXTERNAL {identity}\r\n").as_bytes())?;
    if !client.line()?.starts_with("OK ") {
        return Err("credential bus authentication failed".into());
    }
    client.write(b"NEGOTIATE_UNIX_FD\r\n")?;
    if client.line()? != "AGREE_UNIX_FD\r" {
        return Err("credential bus refused descriptors".into());
    }
    client.write(b"BEGIN\r\n")?;
    let hello = client.call(BUS, "/org/freedesktop/DBus", BUS, "Hello", None, Some(BUS))?;
    let unique = one_string(&hello)?;
    if !name::valid_unique_name(&unique) {
        return Err("invalid credential client identity".into());
    }
    let owner = client.call(
        BUS,
        "/org/freedesktop/DBus",
        BUS,
        "GetNameOwner",
        Some(PORTAL),
        Some(BUS),
    )?;
    let owner = one_string(&owner)?;
    if !name::valid_unique_name(&owner) {
        return Err("invalid credential portal identity".into());
    }
    let mut frame = client.call(
        PORTAL,
        "/org/freedesktop/portal/desktop",
        "td.Secret1",
        "Retrieve",
        Some(name),
        Some(&owner),
    )?;
    let (reply, _) =
        message::decode(&frame.bytes, frame.fds.0.len() as u32).map_err(|e| e.to_string())?;
    if reply.fields.signature != Some("hs")
        || reply.fields.destination != Some(unique.as_str())
        || frame.fds.0.len() != 1
    {
        return Err("invalid credential descriptor reply".into());
    }
    let receipt = match reply.args() {
        [_, wire::Value::Str(token)]
            if token.len() == 32 && token.bytes().all(|b| b.is_ascii_hexdigit()) =>
        {
            (*token).to_string()
        }
        _ => return Err("invalid credential receipt token".into()),
    };
    let fd = frame.fds.0.pop().ok_or("credential descriptor is absent")?;
    let file: File = sys::duplicate_received(fd)?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file()
        || metadata.nlink() != 0
        || metadata.len() == 0
        || metadata.len() > store::MAX_SECRET as u64
    {
        return Err("invalid credential backing file".into());
    }
    let mut bytes = Vec::new();
    file.take((store::MAX_SECRET + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() != metadata.len() as usize {
        return Err("credential size changed during transfer".into());
    }
    let acknowledgement = client.call(
        PORTAL,
        "/org/freedesktop/portal/desktop",
        "td.Secret1",
        "Received",
        Some(&receipt),
        Some(&owner),
    )?;
    let (reply, _) = message::decode(&acknowledgement.bytes, acknowledgement.fds.0.len() as u32)
        .map_err(|e| e.to_string())?;
    if !reply.args().is_empty()
        || !acknowledgement.fds.0.is_empty()
        || reply.fields.destination != Some(unique.as_str())
    {
        return Err("invalid credential acknowledgement".into());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::net::UnixListener;

    #[test]
    fn live_descriptor_exchange_checks_bytes_sender_and_receipt() {
        for forged in [false, true] {
            let root = std::env::temp_dir()
                .join(format!("td-secret-client-{}-{forged}", std::process::id()));
            fs::create_dir_all(&root).unwrap();
            let socket = root.join("bus");
            let listener = UnixListener::bind(&socket).unwrap();
            let file_path = root.join("credential");
            let mut file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&file_path)
                .unwrap();
            fs::remove_file(&file_path).unwrap();
            file.write_all(b"a credential\n").unwrap();
            let file = File::open(format!("/proc/self/fd/{}", file.as_raw_fd())).unwrap();
            let server = std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                let mut server = Client {
                    stream,
                    deadline: Instant::now() + Duration::from_secs(5),
                    serial: 0,
                };
                assert!(server.line().unwrap().starts_with("\0AUTH EXTERNAL "));
                server
                    .write(b"OK 01234567890123456789012345678901\r\n")
                    .unwrap();
                assert_eq!(server.line().unwrap(), "NEGOTIATE_UNIX_FD\r");
                server.write(b"AGREE_UNIX_FD\r\n").unwrap();
                assert_eq!(server.line().unwrap(), "BEGIN\r");
                for (member, value) in [("Hello", ":1.10"), ("GetNameOwner", ":1.20")] {
                    let frame = server.frame().unwrap();
                    let (call, _) = message::decode(&frame.bytes, 0).unwrap();
                    assert_eq!(call.fields.member, Some(member));
                    let reply = message::Builder::method_return(wire::Endian::Little, call.serial)
                        .serial(call.serial)
                        .sender(BUS)
                        .destination(":1.10")
                        .body("s", |w| w.string(value))
                        .unwrap()
                        .encode()
                        .unwrap();
                    server.write(&reply).unwrap();
                }
                let frame = server.frame().unwrap();
                let (call, _) = message::decode(&frame.bytes, 0).unwrap();
                assert_eq!(call.fields.member, Some("Retrieve"));
                assert_eq!(call.args(), &[wire::Value::Str("main")]);
                let token = "01234567890123456789012345678901";
                let reply = message::Builder::method_return(wire::Endian::Big, call.serial)
                    .serial(30)
                    .sender(if forged { ":1.99" } else { ":1.20" })
                    .destination(":1.10")
                    .unix_fds(1)
                    .body("hs", |w| {
                        w.uint32(0);
                        w.string(token)
                    })
                    .unwrap()
                    .encode()
                    .unwrap();
                sys::send_with_fd(&server.stream, &reply[..1], file.as_raw_fd()).unwrap();
                server.write(&reply[1..]).unwrap();
                if forged {
                    return;
                }
                let ack = server.frame().unwrap();
                let (ack, _) = message::decode(&ack.bytes, 0).unwrap();
                assert_eq!(ack.fields.member, Some("Received"));
                assert_eq!(ack.args(), &[wire::Value::Str(token)]);
                server
                    .write(
                        &message::Builder::method_return(wire::Endian::Little, ack.serial)
                            .serial(31)
                            .sender(":1.20")
                            .destination(":1.10")
                            .encode()
                            .unwrap(),
                    )
                    .unwrap();
            });
            let result = retrieve_at(socket, "main");
            if forged {
                assert!(result.unwrap_err().contains("wrong sender"));
            } else {
                assert_eq!(result.unwrap(), b"a credential\n");
            }
            server.join().unwrap();
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn descriptor_transport_is_shared_and_disposal_is_owned() {
        let main = include_str!("main.rs");
        assert!(main.contains("mod sys;"));
        let source = include_str!("client.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        for site in [
            "sys::recv_with_fds(",
            "sys::duplicate_received(",
            "sys::discard_received(",
        ] {
            assert_eq!(source.matches(site).count(), 1);
        }
        assert!(!source.contains("from_raw_fd"));
    }
}
