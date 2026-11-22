//! Links other programs open in td-web (DESIGN.md §7): with `WEB_LINKS_FROM`
//! set to `portal`, as td's `web` application sets it, td-web holds a name
//! under its `org.td.web.*` grant on the session bus and takes
//! `OpenURL(s)` at `/org/td/web` from the desktop portal alone: each call's
//! sender must be the bus's present owner of
//! `org.freedesktop.portal.Desktop`, which the broker reserves to
//! td-portal. The portal chooses td-web per request and authenticates it
//! by its broker credentials before calling (APPLICATIONS.md §E row 4);
//! this side refuses every other caller, the login user's own unjailed
//! processes included. A link taken goes to the window, which opens it as
//! a typed address. No descriptor is taken, no signal sent, nothing else
//! served.

use crate::bus_client;
use crate::message::{self, Builder, Message, MessageType};
use crate::wire::{Endian, Value};
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::fmt;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant};

/// The setting that turns the intake on, and its one value.
pub const SETTING: &str = "WEB_LINKS_FROM";
const FROM_PORTAL: &str = "portal";
/// Where the portal finds td-web: names under the grant's base, the object
/// and the method.
pub const NAME_BASE: &str = "org.td.web";
pub const PATH: &str = "/org/td/web";
pub const INTERFACE: &str = "org.td.web";
pub const MEMBER: &str = "OpenURL";
const PORTAL: &str = "org.freedesktop.portal.Desktop";
const BUS: &str = "org.freedesktop.DBus";
const BUS_PATH: &str = "/org/freedesktop/DBus";
const DO_NOT_QUEUE: u32 = 4;
const PRIMARY_OWNER: u32 = 1;
const UNKNOWN_METHOD: &str = "org.freedesktop.DBus.Error.UnknownMethod";
const INVALID_ARGS: &str = "org.freedesktop.DBus.Error.InvalidArgs";
const ACCESS_DENIED: &str = "org.freedesktop.DBus.Error.AccessDenied";
const LIMITS: &str = "org.freedesktop.DBus.Error.LimitsExceeded";
const FAILED: &str = "org.freedesktop.DBus.Error.Failed";
/// The portal's own link bound (td-portal/src/open_uri.rs).
const MAX_URL_BYTES: usize = 8192;
/// The largest frame read whole; a larger one, which no portal call is,
/// is read past and dropped.
const MAX_FRAME: usize = 64 * 1024;
const MAX_AUTH_LINE: usize = 512;
/// Frames read while waiting on one of td-web's own calls.
const MAX_UNRELATED: usize = 64;
/// How long each of td-web's own bus exchanges may take, all its frames.
const EXCHANGE: Duration = Duration::from_secs(5);
/// Links waiting for the window, and calls waiting while td-web asks the
/// bus something: one more of either is refused as busy.
const WAITING: usize = 4;
/// Why the intake stopped, said once on stderr: a fixed text, so a
/// failure allocates nothing to say it.
type Error = &'static str;
const UNDECODED: Error = "a bus frame did not decode";
const UNENCODED: Error = "a bus message could not be encoded";
const LATE: Error = "the session bus did not answer in time";

/// Starts taking links when `setting` asks for it and `address` names a
/// unix-path bus: the window's end of the links taken, else none. A bus
/// that cannot be reached or refuses every name is said on stderr; td-web
/// runs on without links.
pub fn start(setting: Option<&OsStr>, address: Option<&str>) -> Option<Receiver<String>> {
    if setting != Some(OsStr::new(FROM_PORTAL)) {
        return None;
    }
    let Some(path) = address.and_then(bus_client::unix_path).map(PathBuf::from) else {
        eprintln!("td-web: links: no unix-path session bus");
        return None;
    };
    let (links, window) = mpsc::sync_channel(WAITING);
    let spawned = std::thread::Builder::new()
        .name(td_alloc::string("td-web-links").ok()?)
        .spawn(move || {
            if let Err(why) = serve(&path, std::process::id(), &links) {
                eprintln!("td-web: links: {why}");
            }
        });
    match spawned {
        Ok(_) => Some(window),
        Err(why) => {
            eprintln!("td-web: links: {why}");
            None
        }
    }
}

/// Connects and serves until the bus or the window goes.
fn serve(path: &Path, pid: u32, links: &SyncSender<String>) -> Result<(), Error> {
    let stream = bus_client::connect_within(path, EXCHANGE, "td-web-links-connect")
        .map_err(|_| "the session bus could not be reached")?;
    let uid = std::fs::metadata("/proc/self")
        .map_err(|_| "td-web's own uid could not be read")?
        .uid();
    run(stream, uid, pid, links)
}

/// Authenticates as `uid`, takes a name and answers calls until the bus
/// or the window goes.
fn run(stream: UnixStream, uid: u32, pid: u32, links: &SyncSender<String>) -> Result<(), Error> {
    let mut bus = Bus {
        stream,
        serial: 0,
        waiting: VecDeque::new(),
    };
    let unique = bus.open(uid)?;
    // The pid first, then the connection's unique name, which the bus
    // never gives twice: a same-uid process holding one name does not keep
    // td-web from the other, though one holding both, foreseen, would.
    let by_pid = td_alloc::format(format_args!("{NAME_BASE}.p{pid}"))
        .map_err(|_| "no memory for the bus name")?;
    let by_unique = td_alloc::format(format_args!("{NAME_BASE}.u{}", Suffix(&unique)))
        .map_err(|_| "no memory for the bus name")?;
    if !bus.take(&by_pid)? && !bus.take(&by_unique)? {
        return Err("the bus gave td-web none of its names");
    }
    loop {
        let frame = match bus.waiting.pop_front() {
            Some(frame) => frame,
            None => match bus.frame(None)? {
                Frame::Whole(frame) => frame,
                Frame::Dropped | Frame::Late => continue,
            },
        };
        let (call, _) = message::decode(&frame, 0).map_err(|_| UNDECODED)?;
        if call.kind == MessageType::MethodCall && !bus.answer(&call, links)? {
            return Ok(());
        }
    }
}

/// A unique name as a bus name's last element: `:1.23` as `1_23`.
struct Suffix<'a>(&'a str);

impl fmt::Display for Suffix<'_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        for c in self.0.chars() {
            match c {
                ':' => {}
                '.' => out.write_str("_")?,
                c if c.is_ascii_alphanumeric() || c == '_' => {
                    fmt::Write::write_char(out, c)?;
                }
                _ => out.write_str("_")?,
            }
        }
        Ok(())
    }
}

/// td-web's connection to the session bus, and the calls to td-web that
/// arrived while it asked the bus something.
struct Bus {
    stream: UnixStream,
    serial: u32,
    waiting: VecDeque<Vec<u8>>,
}

/// The bus's answer to one of td-web's calls: its return, a refusal, or
/// none in time, among too much other traffic or past the deadline, the
/// stream still between frames.
enum Answer {
    Returned(Vec<u8>),
    Refused,
    Unanswered,
}

/// A frame read: whole, read past as too large, or not begun by the
/// deadline, the stream still between frames.
enum Frame {
    Whole(Vec<u8>),
    Dropped,
    Late,
}

/// What an `OpenURL` call asks, or the error it is answered with.
enum Asked<'a> {
    Link(&'a str),
    Refused(&'static str, &'static str),
}

/// What a call to td-web asks: `OpenURL` with one `http` or `https` link
/// as the portal admits it; any other member of td-web's is unknown.
fn asked<'a>(call: &Message<'a>) -> Asked<'a> {
    let fields = &call.fields;
    if fields.path != Some(PATH)
        || fields.interface != Some(INTERFACE)
        || fields.member != Some(MEMBER)
    {
        return Asked::Refused(UNKNOWN_METHOD, "td-web serves OpenURL alone");
    }
    match (fields.signature, call.args()) {
        (Some("s"), [Value::Str(url)]) if link(url) => Asked::Link(url),
        _ => Asked::Refused(INVALID_ARGS, "OpenURL takes one http or https link"),
    }
}

/// Whether `url` is a link the window is handed: `http://` or `https://`
/// in any case, no whitespace or control character, within the portal's
/// bound, as the portal admits a link. The window reads it as a typed
/// address, which td-url encodes or refuses further.
fn link(url: &str) -> bool {
    let scheme = url.split_once("://").map(|(scheme, _)| scheme);
    url.len() <= MAX_URL_BYTES
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
        && scheme.is_some_and(|s| s.eq_ignore_ascii_case("http") || s.eq_ignore_ascii_case("https"))
}

impl Bus {
    /// Sets the stream's timeouts: `deadline`'s remainder, or none to wait
    /// for the next call; a write is bounded either way.
    fn within(&mut self, deadline: Option<Instant>) -> Result<(), Error> {
        let left = match deadline {
            Some(deadline) => Some(
                deadline
                    .checked_duration_since(Instant::now())
                    .filter(|left| !left.is_zero())
                    .ok_or(LATE)?,
            ),
            None => None,
        };
        self.stream
            .set_read_timeout(left)
            .and_then(|()| self.stream.set_write_timeout(left.or(Some(EXCHANGE))))
            .map_err(|_| "the bus connection's timeouts could not be set")
    }

    fn write(&mut self, mut bytes: &[u8]) -> Result<(), Error> {
        while !bytes.is_empty() {
            match self.stream.write(bytes) {
                Ok(0) => return Err("the session bus closed"),
                Ok(written) => bytes = bytes.get(written..).unwrap_or_default(),
                Err(why) if why.kind() == ErrorKind::Interrupted => {}
                Err(_) => return Err("writing to the session bus failed"),
            }
        }
        Ok(())
    }

    fn read(&mut self, mut bytes: &mut [u8]) -> Result<(), Error> {
        while !bytes.is_empty() {
            match self.stream.read(bytes) {
                Ok(0) => return Err("the session bus closed"),
                Ok(read) => bytes = bytes.get_mut(read..).unwrap_or_default(),
                Err(why) if why.kind() == ErrorKind::Interrupted => {}
                Err(_) => return Err("reading from the session bus failed"),
            }
        }
        Ok(())
    }

    /// One line of the authentication exchange, without its line end.
    fn line(&mut self) -> Result<[u8; MAX_AUTH_LINE], Error> {
        let mut line = [0u8; MAX_AUTH_LINE];
        for at in 0..MAX_AUTH_LINE {
            let mut byte = [0u8; 1];
            self.read(&mut byte)?;
            if byte == *b"\n" {
                return Ok(line);
            }
            if let Some(slot) = line.get_mut(at) {
                *slot = byte[0];
            }
        }
        Err("an oversized bus authentication line")
    }

    /// Authenticates as `uid`, taking no descriptors, and says hello: the
    /// unique name the bus gave td-web.
    fn open(&mut self, uid: u32) -> Result<String, Error> {
        self.within(Some(deadline()?))?;
        self.write(bus_client::auth_line(uid).as_bytes())?;
        if !self.line()?.starts_with(b"OK ") {
            return Err("the session bus refused authentication");
        }
        self.write(b"BEGIN\r\n")?;
        let call =
            Builder::method_call(Endian::Little, BUS_PATH, Some(BUS), "Hello").destination(BUS);
        let Answer::Returned(reply) = self.call(call)? else {
            return Err("the session bus did not say hello to td-web");
        };
        let (reply, _) = message::decode(&reply, 0).map_err(|_| UNDECODED)?;
        match reply.args() {
            [Value::Str(unique)] if crate::name::valid_unique_name(unique) => {
                td_alloc::string(unique).map_err(|_| "no memory for the unique name")
            }
            _ => Err("the session bus gave td-web no unique name"),
        }
    }

    /// Asks for `name`, td-web's alone: whether the bus gave it.
    fn take(&mut self, name: &str) -> Result<bool, Error> {
        let call = Builder::method_call(Endian::Little, BUS_PATH, Some(BUS), "RequestName")
            .destination(BUS)
            .body("su", |writer| {
                writer.string(name)?;
                writer.uint32(DO_NOT_QUEUE);
                Ok(())
            })
            .map_err(|_| UNENCODED)?;
        let Answer::Returned(reply) = self.call(call)? else {
            return Ok(false);
        };
        let (reply, _) = message::decode(&reply, 0).map_err(|_| UNDECODED)?;
        Ok(matches!(reply.args(), [value] if value.as_u32() == Some(PRIMARY_OWNER)))
    }

    /// Whether `sender`, a unique name, owns `name` now: no owner, which
    /// the bus answers with an error, is no; none when the bus's answer
    /// did not come in time.
    fn owns(&mut self, sender: &str, name: &str) -> Result<Option<bool>, Error> {
        let call = Builder::method_call(Endian::Little, BUS_PATH, Some(BUS), "GetNameOwner")
            .destination(BUS)
            .body("s", |writer| writer.string(name))
            .map_err(|_| UNENCODED)?;
        let reply = match self.call(call)? {
            Answer::Returned(reply) => reply,
            Answer::Refused => return Ok(Some(false)),
            Answer::Unanswered => return Ok(None),
        };
        let (reply, _) = message::decode(&reply, 0).map_err(|_| UNDECODED)?;
        Ok(Some(
            matches!(reply.args(), [Value::Str(owner)] if *owner == sender)
                && crate::name::valid_unique_name(sender),
        ))
    }

    /// The next frame, read by `deadline` or waited for. One larger than
    /// `MAX_FRAME` is read past and dropped; one not begun by the deadline
    /// is late, and the stream is still between frames. A deadline passing
    /// inside a frame is fatal.
    fn frame(&mut self, deadline: Option<Instant>) -> Result<Frame, Error> {
        match self.within(deadline) {
            Err(LATE) => return Ok(Frame::Late),
            other => other?,
        }
        let mut head = [0u8; message::HEADER_LEN];
        let first = loop {
            match self.stream.read(&mut head) {
                Ok(0) => return Err("the session bus closed"),
                Ok(read) => break read,
                Err(why) if why.kind() == ErrorKind::Interrupted => {}
                Err(why) if matches!(why.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return Ok(Frame::Late);
                }
                Err(_) => return Err("reading from the session bus failed"),
            }
        };
        self.read(head.get_mut(first..).unwrap_or_default())?;
        let total = message::frame_len(&head)
            .map_err(|_| UNDECODED)?
            .ok_or("a short bus header")?;
        let rest = total
            .checked_sub(message::HEADER_LEN)
            .ok_or("an invalid bus frame length")?;
        if total > MAX_FRAME {
            let mut left = rest;
            let mut scratch = [0u8; 4096];
            while left > 0 {
                let part = left.min(scratch.len());
                self.read(scratch.get_mut(..part).unwrap_or_default())?;
                left -= part;
            }
            return Ok(Frame::Dropped);
        }
        let mut bytes = td_alloc::filled(total, 0u8).map_err(|_| "no memory for a bus frame")?;
        if let Some(start) = bytes.get_mut(..message::HEADER_LEN) {
            start.copy_from_slice(&head);
        }
        self.read(bytes.get_mut(message::HEADER_LEN..).unwrap_or_default())?;
        Ok(Frame::Whole(bytes))
    }

    /// Sends a call to the bus and returns its answer, every frame read by
    /// one deadline. A call to td-web that arrives meanwhile waits to be
    /// answered after, up to `WAITING` of them; one more is refused as
    /// busy. Anything else, a reply the bus did not send among it, is
    /// passed over. Past the deadline, or `MAX_UNRELATED` frames, the call
    /// is unanswered; its late answer, should it come, is passed over as
    /// any reply is.
    fn call(&mut self, call: Builder<'_>) -> Result<Answer, Error> {
        self.serial = self.serial.checked_add(1).ok_or("bus serials ran out")?;
        let serial = self.serial;
        let deadline = deadline()?;
        self.within(Some(deadline))?;
        let frame = call.serial(serial).encode().map_err(|_| UNENCODED)?;
        self.write(&frame)?;
        for _ in 0..MAX_UNRELATED {
            let frame = match self.frame(Some(deadline))? {
                Frame::Whole(frame) => frame,
                Frame::Dropped => continue,
                Frame::Late => return Ok(Answer::Unanswered),
            };
            let (reply, _) = message::decode(&frame, 0).map_err(|_| UNDECODED)?;
            if reply.kind == MessageType::MethodCall {
                if self.waiting.len() < WAITING {
                    td_alloc::push_back(&mut self.waiting, frame)
                        .map_err(|_| "no memory for a waiting call")?;
                } else {
                    self.refuse(&reply, LIMITS, "td-web is busy: ask again")?;
                }
                continue;
            }
            if reply.fields.reply_serial != Some(serial) || reply.fields.sender != Some(BUS) {
                continue;
            }
            return match reply.kind {
                MessageType::MethodReturn => Ok(Answer::Returned(frame)),
                MessageType::Error => Ok(Answer::Refused),
                _ => Err("the bus answered td-web's call with neither"),
            };
        }
        Ok(Answer::Unanswered)
    }

    /// Answers a call to td-web: whether the window is still there to take
    /// links.
    fn answer(&mut self, call: &Message<'_>, links: &SyncSender<String>) -> Result<bool, Error> {
        let url = match asked(call) {
            Asked::Link(url) => url,
            Asked::Refused(error, text) => return self.refuse(call, error, text).map(|()| true),
        };
        let portal = match call.fields.sender {
            Some(sender) => self.owns(sender, PORTAL)?,
            None => Some(false),
        };
        let Some(portal) = portal else {
            let text = "td-web could not ask the bus who the portal is: ask again";
            return self.refuse(call, FAILED, text).map(|()| true);
        };
        if !portal {
            let text = "td-web takes links from the desktop portal alone";
            return self.refuse(call, ACCESS_DENIED, text).map(|()| true);
        }
        let taken = td_alloc::string(url).map_err(|_| "no memory for a link")?;
        match links.try_send(taken) {
            Ok(()) => self.reply(call).map(|()| true),
            Err(TrySendError::Full(_)) => {
                let text = "td-web has links waiting: ask again";
                self.refuse(call, LIMITS, text).map(|()| true)
            }
            Err(TrySendError::Disconnected(_)) => {
                let text = "td-web's window has closed";
                self.refuse(call, FAILED, text).map(|()| false)
            }
        }
    }

    fn reply(&mut self, call: &Message<'_>) -> Result<(), Error> {
        if call.flags & message::FLAG_NO_REPLY_EXPECTED != 0 {
            return Ok(());
        }
        let Some(sender) = call.fields.sender else {
            return Ok(());
        };
        let reply = Builder::method_return(Endian::Little, call.serial).destination(sender);
        self.send(reply)
    }

    fn refuse(&mut self, call: &Message<'_>, error: &str, text: &str) -> Result<(), Error> {
        if call.flags & message::FLAG_NO_REPLY_EXPECTED != 0 {
            return Ok(());
        }
        let Some(sender) = call.fields.sender else {
            return Ok(());
        };
        let reply = Builder::error(Endian::Little, error, call.serial)
            .destination(sender)
            .body("s", |writer| writer.string(text))
            .map_err(|_| UNENCODED)?;
        self.send(reply)
    }

    fn send(&mut self, message: Builder<'_>) -> Result<(), Error> {
        self.within(None)?;
        self.serial = self.serial.checked_add(1).ok_or("bus serials ran out")?;
        let frame = message
            .serial(self.serial)
            .encode()
            .map_err(|_| UNENCODED)?;
        self.write(&frame)
    }
}

/// When one of td-web's own exchanges must be over.
fn deadline() -> Result<Instant, Error> {
    Instant::now().checked_add(EXCHANGE).ok_or(LATE)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use std::thread;

    const PORTAL_UNIQUE: &str = ":1.5";
    const WEB_UNIQUE: &str = ":1.9";
    const EXISTS: u32 = 3;

    /// The broker's end: reads td-web's frames and answers them.
    struct Broker {
        stream: UnixStream,
        serial: u32,
    }

    impl Broker {
        fn line(&mut self) -> String {
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                self.stream.read_exact(&mut byte).unwrap();
                if byte[0] == b'\n' {
                    return String::from_utf8(line).unwrap();
                }
                line.push(byte[0]);
            }
        }

        fn frame(&mut self) -> Vec<u8> {
            let mut head = [0u8; 16];
            self.stream.read_exact(&mut head).unwrap();
            let total = message::frame_len(&head).unwrap().unwrap();
            let mut bytes = head.to_vec();
            bytes.resize(total, 0);
            self.stream.read_exact(&mut bytes[16..]).unwrap();
            bytes
        }

        fn send(&mut self, builder: Builder<'_>) {
            self.serial += 1;
            let frame = builder.serial(self.serial).encode().unwrap();
            self.stream.write_all(&frame).unwrap();
        }

        /// Reads td-web's next call to the bus, `member`, and answers it
        /// with a string, or as an error when none: the call it read.
        fn answer(&mut self, member: &str, body: Option<&str>) -> Vec<u8> {
            let frame = self.frame();
            let (call, _) = message::decode(&frame, 0).unwrap();
            assert_eq!(call.fields.member, Some(member));
            assert_eq!(call.fields.destination, Some(BUS));
            self.reply_to(call.serial, body);
            frame
        }

        fn reply_to(&mut self, serial: u32, body: Option<&str>) {
            let reply = match body {
                Some(text) => Builder::method_return(Endian::Little, serial)
                    .sender(BUS)
                    .destination(WEB_UNIQUE)
                    .body("s", |writer| writer.string(text))
                    .unwrap(),
                None => Builder::error(
                    Endian::Little,
                    "org.freedesktop.DBus.Error.NameHasNoOwner",
                    serial,
                )
                .sender(BUS)
                .destination(WEB_UNIQUE),
            };
            self.send(reply);
        }

        /// Reads td-web's request for a name and answers `code`: the name.
        fn name(&mut self, code: u32) -> String {
            let frame = self.frame();
            let (call, _) = message::decode(&frame, 0).unwrap();
            assert_eq!(call.fields.member, Some("RequestName"));
            assert_eq!(call.args()[1].as_u32(), Some(DO_NOT_QUEUE));
            let reply = Builder::method_return(Endian::Little, call.serial)
                .sender(BUS)
                .destination(WEB_UNIQUE)
                .body("u", |writer| {
                    writer.uint32(code);
                    Ok(())
                })
                .unwrap();
            self.send(reply);
            call.args()[0].as_str().unwrap().to_string()
        }

        /// The handshake and Hello, as a broker gives them.
        fn hello(&mut self) {
            assert!(self.line().starts_with("\0AUTH EXTERNAL "));
            self.stream
                .write_all(b"OK 0123456789abcdef0123456789abcdef\r\n")
                .unwrap();
            assert_eq!(self.line(), "BEGIN\r");
            self.answer("Hello", Some(WEB_UNIQUE));
        }

        /// Hello and the first name given: the name.
        fn open(&mut self) -> String {
            self.hello();
            self.name(PRIMARY_OWNER)
        }

        fn call(sender: &str, url: &str) -> Builder<'static> {
            let url = url.to_string();
            let sender = sender.to_string();
            Builder::method_call(Endian::Little, PATH, Some(INTERFACE), MEMBER)
                .sender(Box::leak(sender.into_boxed_str()))
                .destination(WEB_UNIQUE)
                .body("s", move |writer| writer.string(&url))
                .unwrap()
        }

        /// A call to td-web from `sender`, and td-web's reply frame once
        /// it has asked the bus who the portal is (`portal`, or nobody).
        fn open_url(&mut self, sender: &str, url: &str, portal: Option<&str>) -> Vec<u8> {
            self.send(Self::call(sender, url));
            if link(url) {
                let asked = self.answer("GetNameOwner", portal);
                let (asked, _) = message::decode(&asked, 0).unwrap();
                assert_eq!(asked.args()[0].as_str(), Some(PORTAL));
            }
            self.frame()
        }
    }

    fn started() -> (
        Broker,
        Receiver<String>,
        thread::JoinHandle<Result<(), Error>>,
    ) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (links, window) = mpsc::sync_channel(WAITING);
        let worker = thread::spawn(move || run(theirs, 1000, 42, &links));
        let broker = Broker {
            stream: ours,
            serial: 0,
        };
        (broker, window, worker)
    }

    fn kind_and_error(frame: &[u8]) -> (MessageType, Option<String>) {
        let (reply, _) = message::decode(frame, 0).unwrap();
        (reply.kind, reply.fields.error_name.map(str::to_string))
    }

    /// The portal's link reaches the window and is answered; td-web holds
    /// its name under the grant's base.
    #[test]
    fn the_portals_link_reaches_the_window() {
        let (mut broker, window, _worker) = started();
        assert_eq!(broker.open(), "org.td.web.p42");
        let reply = broker.open_url(PORTAL_UNIQUE, "https://e.example/a", Some(PORTAL_UNIQUE));
        assert_eq!(kind_and_error(&reply), (MessageType::MethodReturn, None));
        assert_eq!(window.try_recv().unwrap(), "https://e.example/a");
    }

    /// A name by the pid held already, td-web asks for one by its unique
    /// name; with neither given, the intake ends.
    #[test]
    fn a_held_name_is_passed_for_the_unique_one() {
        let (mut broker, window, _worker) = started();
        broker.hello();
        assert_eq!(broker.name(EXISTS), "org.td.web.p42");
        assert_eq!(broker.name(PRIMARY_OWNER), "org.td.web.u1_9");
        let reply = broker.open_url(PORTAL_UNIQUE, "https://e.example/a", Some(PORTAL_UNIQUE));
        assert_eq!(kind_and_error(&reply).0, MessageType::MethodReturn);
        assert!(window.try_recv().is_ok());
        let (mut broker, _window, worker) = started();
        broker.hello();
        broker.name(EXISTS);
        broker.name(EXISTS);
        assert_eq!(
            worker.join().unwrap(),
            Err("the bus gave td-web none of its names")
        );
    }

    /// Any caller but the portal's present owner is refused, the login
    /// user's own processes as much as another's, and while no portal
    /// runs, every caller; nothing reaches the window.
    #[test]
    fn a_caller_other_than_the_portal_is_refused() {
        let (mut broker, window, _worker) = started();
        broker.open();
        for (sender, portal) in [(":1.7", Some(PORTAL_UNIQUE)), (PORTAL_UNIQUE, None)] {
            let reply = broker.open_url(sender, "https://e.example/a", portal);
            assert_eq!(
                kind_and_error(&reply),
                (MessageType::Error, Some(ACCESS_DENIED.to_string()))
            );
        }
        assert!(window.try_recv().is_err());
    }

    /// A reply to td-web's question that the bus did not send is passed
    /// over: a peer cannot answer for the bus who the portal is.
    #[test]
    fn a_forged_answer_is_passed_over() {
        let (mut broker, window, _worker) = started();
        broker.open();
        broker.send(Broker::call(":1.7", "https://e.example/a"));
        let asked = broker.frame();
        let (asked, _) = message::decode(&asked, 0).unwrap();
        assert_eq!(asked.fields.member, Some("GetNameOwner"));
        let forged = Builder::method_return(Endian::Little, asked.serial)
            .sender(":1.7")
            .destination(WEB_UNIQUE)
            .body("s", |writer| writer.string(":1.7"))
            .unwrap();
        broker.send(forged);
        broker.reply_to(asked.serial, Some(PORTAL_UNIQUE));
        assert_eq!(
            kind_and_error(&broker.frame()).1.as_deref(),
            Some(ACCESS_DENIED)
        );
        assert!(window.try_recv().is_err());
    }

    /// A call that arrives while td-web asks the bus about another waits
    /// and is answered after it, both links reaching the window.
    #[test]
    fn a_call_during_a_question_waits_its_turn() {
        let (mut broker, window, _worker) = started();
        broker.open();
        broker.send(Broker::call(PORTAL_UNIQUE, "https://e.example/1"));
        let asked = broker.frame();
        let (asked, _) = message::decode(&asked, 0).unwrap();
        broker.send(Broker::call(PORTAL_UNIQUE, "https://e.example/2"));
        broker.reply_to(asked.serial, Some(PORTAL_UNIQUE));
        assert_eq!(kind_and_error(&broker.frame()).0, MessageType::MethodReturn);
        broker.answer("GetNameOwner", Some(PORTAL_UNIQUE));
        assert_eq!(kind_and_error(&broker.frame()).0, MessageType::MethodReturn);
        assert_eq!(window.try_recv().unwrap(), "https://e.example/1");
        assert_eq!(window.try_recv().unwrap(), "https://e.example/2");
    }

    /// A burst of frames during td-web's question about the portal leaves
    /// the call refused, to be asked again, and the intake running: the
    /// bus's late answer is passed over.
    #[test]
    fn a_burst_during_a_question_is_borne() {
        let (mut broker, window, _worker) = started();
        broker.open();
        broker.send(Broker::call(PORTAL_UNIQUE, "https://e.example/first"));
        let asked = broker.frame();
        let (asked, _) = message::decode(&asked, 0).unwrap();
        for _ in 0..MAX_UNRELATED + 6 {
            let signal = Builder::signal(Endian::Little, "/x", "org.e.X", "Noise")
                .sender(":1.7")
                .destination(WEB_UNIQUE);
            broker.send(signal);
        }
        assert_eq!(kind_and_error(&broker.frame()).1.as_deref(), Some(FAILED));
        broker.reply_to(asked.serial, Some(PORTAL_UNIQUE));
        let reply = broker.open_url(PORTAL_UNIQUE, "https://e.example/b", Some(PORTAL_UNIQUE));
        assert_eq!(kind_and_error(&reply).0, MessageType::MethodReturn);
        assert_eq!(window.try_recv().unwrap(), "https://e.example/b");
    }

    /// A call that asks for no reply gets none; a frame past `MAX_FRAME`
    /// is read past and dropped, and the intake goes on.
    #[test]
    fn a_silent_call_and_an_oversized_frame_are_borne() {
        let (mut broker, window, _worker) = started();
        broker.open();
        broker.send(
            Broker::call(PORTAL_UNIQUE, "https://e.example/quiet")
                .flags(message::FLAG_NO_REPLY_EXPECTED),
        );
        broker.answer("GetNameOwner", Some(PORTAL_UNIQUE));
        let big = "a".repeat(MAX_FRAME + 1);
        let signal = Builder::signal(Endian::Little, "/x", "org.e.X", "Big")
            .sender(":1.7")
            .destination(WEB_UNIQUE)
            .body("s", |writer| writer.string(&big))
            .unwrap();
        broker.send(signal);
        // The next frame td-web writes is its question for the next call,
        // not a reply to the silent one.
        let reply = broker.open_url(PORTAL_UNIQUE, "https://e.example/b", Some(PORTAL_UNIQUE));
        assert_eq!(kind_and_error(&reply).0, MessageType::MethodReturn);
        assert_eq!(window.try_recv().unwrap(), "https://e.example/quiet");
        assert_eq!(window.try_recv().unwrap(), "https://e.example/b");
    }

    /// Only an `http` or `https` link without whitespace or controls,
    /// within the bound, is taken, as the portal admits one; another
    /// member is unknown.
    #[test]
    fn only_a_link_is_taken() {
        let (mut broker, window, _worker) = started();
        broker.open();
        let long = format!("https://e.example/{}", "a".repeat(MAX_URL_BYTES));
        for url in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://e.example/a b",
            "https://e.example/\u{7}",
            "https:e.example",
            &long,
        ] {
            let reply = broker.open_url(PORTAL_UNIQUE, url, Some(PORTAL_UNIQUE));
            assert_eq!(
                kind_and_error(&reply),
                (MessageType::Error, Some(INVALID_ARGS.to_string())),
                "{url}"
            );
        }
        let call = Builder::method_call(Endian::Little, PATH, Some(INTERFACE), "Quit")
            .sender(PORTAL_UNIQUE)
            .destination(WEB_UNIQUE);
        broker.send(call);
        assert_eq!(
            kind_and_error(&broker.frame()),
            (MessageType::Error, Some(UNKNOWN_METHOD.to_string()))
        );
        assert!(window.try_recv().is_err());
        assert!(link("HTTP://E.example/"));
        assert!(link("https://e.example/caf\u{e9}"));
    }

    /// Links past those waiting for the window are refused as busy; once
    /// the window has gone, the intake ends.
    #[test]
    fn a_full_or_closed_window_refuses() {
        let (mut broker, window, worker) = started();
        broker.open();
        for _ in 0..WAITING {
            let reply = broker.open_url(PORTAL_UNIQUE, "https://e.example/", Some(PORTAL_UNIQUE));
            assert_eq!(kind_and_error(&reply).0, MessageType::MethodReturn);
        }
        let reply = broker.open_url(PORTAL_UNIQUE, "https://e.example/", Some(PORTAL_UNIQUE));
        assert_eq!(kind_and_error(&reply).1.as_deref(), Some(LIMITS));
        drop(window);
        let reply = broker.open_url(PORTAL_UNIQUE, "https://e.example/", Some(PORTAL_UNIQUE));
        assert_eq!(kind_and_error(&reply).1.as_deref(), Some(FAILED));
        assert_eq!(worker.join().unwrap(), Ok(()));
    }

    /// Off unless the setting asks, and only with a unix-path bus.
    #[test]
    fn the_intake_is_off_unless_asked() {
        assert!(start(None, Some("unix:path=/nonexistent/bus")).is_none());
        assert!(start(Some(OsStr::new("yes")), Some("unix:path=/nonexistent/bus")).is_none());
        assert!(start(Some(OsStr::new(FROM_PORTAL)), Some("tcp:host=e.example")).is_none());
        assert!(start(Some(OsStr::new(FROM_PORTAL)), None).is_none());
    }
}
