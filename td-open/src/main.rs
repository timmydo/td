#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

//! td-open — hands one `http` or `https` link to the browser through the
//! desktop portal's `OpenURI` and waits for its answer (APPLICATIONS.md
//! §W.6). A jailed application's manifest names it as `$BROWSER`, which
//! td-ui's link opener runs with the link as its one argument; the portal,
//! not this program, decides what is a link and who opens it.

#[path = "../../td-busd/src/message.rs"]
#[allow(
    dead_code,
    reason = "the shared broker codec is broader than one client"
)]
mod message;
#[path = "../../td-busd/src/name.rs"]
#[allow(
    dead_code,
    reason = "the shared broker codec is broader than one client"
)]
mod name;
#[path = "../../td-busd/src/wire.rs"]
#[allow(
    dead_code,
    reason = "the shared broker codec is broader than one client"
)]
mod wire;

use message::{Message, MessageType};
use std::ffi::OsString;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};
use wire::{Endian, Value};

const USAGE: &str = "usage: td-open LINK";
const BUS: &str = "org.freedesktop.DBus";
const BUS_PATH: &str = "/org/freedesktop/DBus";
const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const OPEN_URI: &str = "org.freedesktop.portal.OpenURI";
const REQUEST: &str = "org.freedesktop.portal.Request";
const TOKEN: &str = "td_open";
const MAX_FRAME: usize = 64 * 1024;
const MAX_AUTH_LINE: usize = 512;
const MAX_UNRELATED: usize = 64;
/// The portal fails a request after 20 seconds; this outlasts it.
const DEADLINE: Duration = Duration::from_secs(30);

fn main() -> ExitCode {
    match run(std::env::args_os().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            let _ = writeln!(std::io::stderr().lock(), "td-open: {why}");
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: Vec<OsString>) -> Result<(), String> {
    let [link] = arguments.as_slice() else {
        return Err(USAGE.into());
    };
    let link = link.to_str().ok_or("the link is not UTF-8")?;
    let address = std::env::var("DBUS_SESSION_BUS_ADDRESS")
        .map_err(|_| "no session bus in this environment")?;
    let uid = fs::metadata("/proc/self")
        .map_err(|e| format!("cannot identify this process: {e}"))?
        .uid();
    open(
        &bus_path(&address)?,
        uid,
        link,
        Instant::now()
            .checked_add(DEADLINE)
            .ok_or("deadline overflow")?,
    )
}

/// The one absolute `unix:path=` socket a session bus address names.
fn bus_path(address: &str) -> Result<PathBuf, String> {
    address
        .strip_prefix("unix:path=")
        .filter(|path| path.starts_with('/') && !path.contains([';', ',', '%', '\0']))
        .map(PathBuf::from)
        .ok_or_else(|| "td-open needs one absolute unix:path session bus".into())
}

/// The Request object the portal exports for `unique`'s `TOKEN`.
fn request_path(unique: &str) -> Option<String> {
    let element = unique.strip_prefix(':')?.replace('.', "_");
    Some(format!("{PORTAL_PATH}/request/{element}/{TOKEN}"))
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
            .filter(|left| !left.is_zero())
            .ok_or_else(|| "the portal did not answer in time".into())
    }

    /// Writes all of `bytes`, each write given only what is left of the
    /// deadline, so a slow peer cannot stretch it.
    fn write(&mut self, mut bytes: &[u8]) -> Result<(), String> {
        while !bytes.is_empty() {
            self.stream
                .set_write_timeout(Some(self.remaining()?))
                .map_err(|e| e.to_string())?;
            match self.stream.write(bytes) {
                Ok(0) => return Err("the session bus closed".into()),
                Ok(written) => bytes = bytes.get(written..).unwrap_or_default(),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(self.failure(&e)),
            }
        }
        Ok(())
    }

    /// Fills `bytes`, each read given only what is left of the deadline.
    fn read(&mut self, mut bytes: &mut [u8]) -> Result<(), String> {
        while !bytes.is_empty() {
            self.stream
                .set_read_timeout(Some(self.remaining()?))
                .map_err(|e| e.to_string())?;
            match self.stream.read(bytes) {
                Ok(0) => return Err("the session bus closed".into()),
                Ok(read) => bytes = bytes.get_mut(read..).unwrap_or_default(),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(self.failure(&e)),
            }
        }
        Ok(())
    }

    /// `error`'s text, or the deadline's when it is why the call failed.
    fn failure(&self, error: &std::io::Error) -> String {
        if Instant::now() >= self.deadline {
            "the portal did not answer in time".into()
        } else {
            error.to_string()
        }
    }

    fn line(&mut self) -> Result<String, String> {
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        while line.len() < MAX_AUTH_LINE {
            self.read(&mut byte)?;
            if byte == *b"\n" {
                return String::from_utf8(line).map_err(|e| e.to_string());
            }
            line.extend_from_slice(&byte);
        }
        Err("oversized bus authentication line".into())
    }

    fn authenticate(&mut self, uid: u32) -> Result<(), String> {
        let identity = uid
            .to_string()
            .bytes()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        self.write(format!("\0AUTH EXTERNAL {identity}\r\n").as_bytes())?;
        if !self.line()?.starts_with("OK ") {
            return Err("the session bus refused authentication".into());
        }
        self.write(b"BEGIN\r\n")
    }

    fn frame(&mut self) -> Result<Vec<u8>, String> {
        let mut head = [0u8; 16];
        self.read(&mut head)?;
        let total = message::frame_len(&head)
            .map_err(|e| e.to_string())?
            .ok_or("short bus header")?;
        if !(16..=MAX_FRAME).contains(&total) {
            return Err("an invalid bus frame length".into());
        }
        let mut bytes = head.to_vec();
        bytes.resize(total, 0);
        self.read(bytes.get_mut(16..).ok_or("missing bus frame tail")?)?;
        Ok(bytes)
    }

    fn next_serial(&mut self) -> Result<u32, String> {
        self.serial = self.serial.checked_add(1).ok_or("bus serial exhausted")?;
        Ok(self.serial)
    }

    /// Sends one call and returns its reply frame from `sender`, skipping
    /// up to `MAX_UNRELATED` other frames.
    fn call(&mut self, sender: &str, call: message::Builder<'_>) -> Result<Vec<u8>, String> {
        let serial = self.next_serial()?;
        self.write(&call.serial(serial).encode().map_err(|e| e.to_string())?)?;
        for _ in 0..MAX_UNRELATED {
            let frame = self.frame()?;
            let (reply, _) = message::decode(&frame, 0).map_err(|e| e.to_string())?;
            if reply.fields.reply_serial != Some(serial) || reply.fields.sender != Some(sender) {
                continue;
            }
            return match reply.kind {
                MessageType::MethodReturn => Ok(frame),
                MessageType::Error => Err(refusal(&reply)),
                _ => Err("an invalid reply type".into()),
            };
        }
        Err("too much unrelated bus traffic".into())
    }

    fn bus_call(&mut self, member: &str, argument: Option<&str>) -> Result<Vec<u8>, String> {
        let call = message::Builder::method_call(Endian::Little, BUS_PATH, Some(BUS), member)
            .destination(BUS);
        let call = match argument {
            Some(argument) => call
                .body("s", |writer| writer.string(argument))
                .map_err(|e| e.to_string())?,
            None => call,
        };
        self.call(BUS, call)
    }
}

/// The remote error's name and, when it carries one, its text.
fn refusal(reply: &Message<'_>) -> String {
    let name = reply.fields.error_name.unwrap_or("an unnamed error");
    match reply.args() {
        [Value::Str(text), ..] => format!("{name}: {text}"),
        _ => name.to_string(),
    }
}

fn one_string(frame: &[u8], signature: &str) -> Result<String, String> {
    let (reply, _) = message::decode(frame, 0).map_err(|e| e.to_string())?;
    match reply.args() {
        [value] if reply.fields.signature == Some(signature) => value
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| "an invalid reply body".into()),
        _ => Err("an invalid reply body".into()),
    }
}

fn open(bus: &Path, uid: u32, link: &str, deadline: Instant) -> Result<(), String> {
    let stream = connect(bus, deadline)?;
    let mut client = Client {
        stream,
        deadline,
        serial: 0,
    };
    client.authenticate(uid)?;
    let unique = one_string(&client.bus_call("Hello", None)?, "s")?;
    if !name::valid_unique_name(&unique) {
        return Err("the bus assigned an invalid name".into());
    }
    let portal = one_string(&client.bus_call("GetNameOwner", Some(PORTAL))?, "s")
        .map_err(|e| format!("no desktop portal: {e}"))?;
    if !name::valid_unique_name(&portal) {
        return Err("the portal has an invalid name".into());
    }
    let path = request_path(&unique).ok_or("cannot derive the portal request")?;
    let call =
        message::Builder::method_call(Endian::Little, PORTAL_PATH, Some(OPEN_URI), "OpenURI")
            .destination(PORTAL)
            .body("ssa{sv}", |writer| {
                writer.string("")?;
                writer.string(link)?;
                writer.array("{sv}", |writer| {
                    writer.dict_entry(|writer| {
                        writer.string("handle_token")?;
                        writer.variant("s", |writer| writer.string(TOKEN))
                    })
                })
            })
            .map_err(|e| e.to_string())?;
    // The portal sends the Response to this connection directly, which the
    // broker delivers without a match rule. It may precede the reply that
    // names its path, so both are read in one loop.
    let serial = client.next_serial()?;
    client.write(&call.serial(serial).encode().map_err(|e| e.to_string())?)?;
    let mut answered = false;
    let mut response = None;
    for _ in 0..MAX_UNRELATED {
        let frame = client.frame()?;
        let (incoming, _) = message::decode(&frame, 0).map_err(|e| e.to_string())?;
        let from_portal = incoming.fields.sender == Some(portal.as_str());
        let from_bus = incoming.fields.sender == Some(BUS);
        let reply = incoming.fields.reply_serial == Some(serial);
        match incoming.kind {
            // The bus answers for a portal that left or a call it refused.
            MessageType::Error if reply && (from_portal || from_bus) => {
                return Err(refusal(&incoming));
            }
            MessageType::MethodReturn if reply && from_portal => match incoming.args() {
                [Value::ObjectPath(object)] if *object == path => answered = true,
                [Value::ObjectPath(_)] => {
                    return Err("the portal answered on another request".into())
                }
                _ => return Err("the portal returned no request".into()),
            },
            MessageType::Signal
                if from_portal
                    && incoming.fields.path == Some(path.as_str())
                    && incoming.fields.interface == Some(REQUEST)
                    && incoming.fields.member == Some("Response") =>
            {
                match incoming.args() {
                    [Value::Uint32(code), Value::Array(_)] => response = Some(*code),
                    _ => return Err("the portal sent a malformed response".into()),
                }
            }
            _ => {}
        }
        if let (true, Some(code)) = (answered, response) {
            return match code {
                0 => Ok(()),
                1 => Err("the request was cancelled".into()),
                _ => Err("the portal could not open the link in the browser".into()),
            };
        }
    }
    Err("too much unrelated bus traffic".into())
}

/// Connects to `bus` by `deadline`: a wedged broker's full backlog would
/// otherwise hold the connect without bound. A connect still pending at
/// the deadline is left to this process's exit.
fn connect(bus: &Path, deadline: Instant) -> Result<UnixStream, String> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let bus = bus.to_path_buf();
    std::thread::Builder::new()
        .name("td-open-connect".into())
        .spawn(move || {
            let _ = sender.send(UnixStream::connect(bus));
        })
        .map_err(|e| format!("cannot reach the session bus: {e}"))?;
    let left = deadline
        .checked_duration_since(Instant::now())
        .unwrap_or_default();
    match receiver.recv_timeout(left) {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(error)) => Err(format!("cannot reach the session bus: {error}")),
        Err(_) => Err("the session bus did not answer in time".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::thread;

    #[test]
    fn the_bus_is_one_absolute_unix_path() {
        assert_eq!(
            bus_path("unix:path=/run/user/1000/bus").unwrap(),
            PathBuf::from("/run/user/1000/bus")
        );
        for address in [
            "",
            "unix:abstract=/tmp/x",
            "unix:path=relative",
            "unix:path=/a,guid=1",
            "unix:path=/a;unix:path=/b",
            "tcp:host=localhost",
        ] {
            assert!(bus_path(address).is_err(), "{address}");
        }
    }

    #[test]
    fn the_request_path_is_the_portals_caller_derived_handle() {
        assert_eq!(
            request_path(":1.42").unwrap(),
            "/org/freedesktop/portal/desktop/request/1_42/td_open"
        );
        assert!(request_path("1.42").is_none());
    }

    #[test]
    fn arguments_are_exactly_one_link() {
        assert_eq!(run(Vec::new()).unwrap_err(), USAGE);
        assert_eq!(run(vec!["a".into(), "b".into()]).unwrap_err(), USAGE);
    }

    /// One message to the client, as the broker would forward it.
    fn frame(
        builder: message::Builder<'_>,
        sender: &str,
        serial: u32,
        signature: &str,
        fill: impl FnOnce(&mut wire::Writer) -> Result<(), wire::WireError>,
    ) -> Vec<u8> {
        builder
            .sender(sender)
            .destination(":1.5")
            .serial(serial)
            .body(signature, fill)
            .unwrap()
            .encode()
            .unwrap()
    }

    fn read_call(stream: &mut UnixStream) -> (Vec<u8>, u32, String) {
        let mut head = [0u8; 16];
        stream.read_exact(&mut head).unwrap();
        let total = message::frame_len(&head).unwrap().unwrap();
        let mut bytes = head.to_vec();
        bytes.resize(total, 0);
        stream.read_exact(&mut bytes[16..]).unwrap();
        let (call, _) = message::decode(&bytes, 0).unwrap();
        let member = call.fields.member.unwrap().to_string();
        let serial = call.serial;
        (bytes, serial, member)
    }

    const HANDLE: &str = "/org/freedesktop/portal/desktop/request/1_5/td_open";

    /// How the fake portal answers OpenURI.
    enum Answer {
        /// Its reply names `path`, and `responses` follow as (sender, code)
        /// Response signals, before the reply when `signals_first`.
        Request {
            path: &'static str,
            responses: Vec<(&'static str, u32)>,
            signals_first: bool,
        },
        /// The bus refuses the call for a portal that left.
        BusError,
    }

    /// A broker and portal in one: answers the client's setup and then its
    /// OpenURI as `answer` says. Returns the OpenURI call it received.
    fn broker(name: &str, answer: Answer) -> (PathBuf, thread::JoinHandle<Vec<u8>>) {
        let root = std::env::temp_dir().join(format!("td-open-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();
        let socket = root.join("bus");
        let listener = UnixListener::bind(&socket).unwrap();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut auth = Vec::new();
            let mut byte = [0u8; 1];
            while !auth.ends_with(b"\r\n") {
                stream.read_exact(&mut byte).unwrap();
                auth.push(byte[0]);
            }
            assert!(auth.starts_with(b"\0AUTH EXTERNAL "));
            stream
                .write_all(b"OK 0123456789abcdef0123456789abcdef\r\n")
                .unwrap();
            let mut begin = [0u8; 7];
            stream.read_exact(&mut begin).unwrap();
            assert_eq!(&begin, b"BEGIN\r\n");
            for (member, owner) in [("Hello", ":1.5"), ("GetNameOwner", ":1.2")] {
                let (_, serial, called) = read_call(&mut stream);
                assert_eq!(called, member);
                let reply = message::Builder::method_return(Endian::Little, serial);
                stream
                    .write_all(&frame(reply, BUS, 1, "s", |w| w.string(owner)))
                    .unwrap();
            }
            let (open_call, serial, member) = read_call(&mut stream);
            assert_eq!(member, "OpenURI");
            let (path, responses, signals_first) = match answer {
                Answer::BusError => {
                    let error = message::Builder::error(
                        Endian::Little,
                        "org.freedesktop.DBus.Error.NoReply",
                        serial,
                    );
                    let bytes = frame(error, BUS, 3, "s", |w| w.string("the portal left"));
                    stream.write_all(&bytes).unwrap();
                    return open_call;
                }
                Answer::Request {
                    path,
                    responses,
                    signals_first,
                } => (path, responses, signals_first),
            };
            let reply = message::Builder::method_return(Endian::Little, serial);
            let reply = frame(reply, ":1.2", 4, "o", |w| w.object_path(path));
            if !signals_first {
                stream.write_all(&reply).unwrap();
            }
            for (index, (sender, code)) in responses.into_iter().enumerate() {
                let signal = message::Builder::signal(Endian::Little, HANDLE, REQUEST, "Response");
                let bytes = frame(signal, sender, 10 + index as u32, "ua{sv}", |w| {
                    w.uint32(code);
                    w.array("{sv}", |_| Ok(()))
                });
                if stream.write_all(&bytes).is_err() {
                    return open_call;
                }
            }
            if signals_first {
                let _ = stream.write_all(&reply);
            }
            open_call
        });
        (socket, worker)
    }

    fn request(responses: Vec<(&'static str, u32)>, signals_first: bool) -> Answer {
        Answer::Request {
            path: HANDLE,
            responses,
            signals_first,
        }
    }

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    fn finish(socket: &Path, worker: thread::JoinHandle<Vec<u8>>) -> Vec<u8> {
        let call = worker.join().unwrap();
        let _ = fs::remove_dir_all(socket.parent().unwrap());
        call
    }

    #[test]
    fn the_link_goes_to_the_portal_and_its_success_is_the_answer() {
        let (socket, worker) = broker("success", request(vec![(":1.9", 2), (":1.2", 0)], false));
        open(&socket, 1000, "https://example.org/a", deadline()).unwrap();
        let call = finish(&socket, worker);
        let (call, _) = message::decode(&call, 0).unwrap();
        assert_eq!(call.fields.destination, Some(PORTAL));
        assert_eq!(call.fields.interface, Some(OPEN_URI));
        assert_eq!(call.fields.signature, Some("ssa{sv}"));
        let [Value::Str(""), Value::Str(link), Value::Array(_)] = call.args() else {
            panic!("OpenURI carries a parent, the link and options");
        };
        assert_eq!(*link, "https://example.org/a");
    }

    #[test]
    fn a_response_before_the_reply_is_still_the_answer() {
        let (socket, worker) = broker("first", request(vec![(":1.2", 0)], true));
        open(&socket, 1000, "https://example.org/a", deadline()).unwrap();
        finish(&socket, worker);
    }

    #[test]
    fn a_failed_or_cancelled_response_is_an_error() {
        for (code, text) in [(2, "could not open"), (1, "cancelled")] {
            let (socket, worker) =
                broker(&format!("code{code}"), request(vec![(":1.2", code)], false));
            let error = open(&socket, 1000, "https://example.org/", deadline()).unwrap_err();
            assert!(error.contains(text), "{error}");
            finish(&socket, worker);
        }
    }

    #[test]
    fn the_bus_answering_for_the_portal_fails_at_once() {
        let (socket, worker) = broker("bus", Answer::BusError);
        let error = open(&socket, 1000, "https://example.org/", deadline()).unwrap_err();
        assert_eq!(error, "org.freedesktop.DBus.Error.NoReply: the portal left");
        finish(&socket, worker);
    }

    #[test]
    fn a_reply_naming_another_request_fails_at_once() {
        let answer = Answer::Request {
            path: "/org/freedesktop/portal/desktop/request/1_5/other",
            responses: Vec::new(),
            signals_first: false,
        };
        let (socket, worker) = broker("other", answer);
        let started = Instant::now();
        let error = open(&socket, 1000, "https://example.org/", deadline()).unwrap_err();
        assert!(error.contains("another request"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(4));
        finish(&socket, worker);
    }

    #[test]
    fn a_silent_bus_is_bounded_by_the_deadline() {
        let root = std::env::temp_dir().join(format!("td-open-{}-silent", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();
        let socket = root.join("bus");
        let _listener = UnixListener::bind(&socket).unwrap();
        let error = open(
            &socket,
            1000,
            "https://example.org/",
            Instant::now() + Duration::from_millis(200),
        )
        .unwrap_err();
        assert!(error.contains("in time"), "{error}");
        let _ = fs::remove_dir_all(&root);
    }
}
