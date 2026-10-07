//! Host lock and suspend events for standalone mode, from the system
//! bus's logind (elogind on Guix System): the invoking session's `Lock`
//! signal and the manager's `PrepareForSleep`. Only signals sent by
//! logind's current owner count; its owner changing, or the bus failing,
//! is the source lost. A "delay" sleep inhibitor, held while watching,
//! holds sleep until the caller drops it after locking; logind's own
//! maximum delay still bounds that wait. Setup and every method call have
//! a deadline; waiting for a signal does not.

use crate::bus_client;
use crate::message::{self, Builder, Message, MessageType};
use crate::sys::{self, ReceiveError};
use crate::wire::{Endian, Value, WireError, Writer};
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::fs::File;
use std::io::Write;
use std::net::Shutdown;
use std::os::fd::RawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const BUS: &str = "org.freedesktop.DBus";
const BUS_PATH: &str = "/org/freedesktop/DBus";
const LOGIN: &str = "org.freedesktop.login1";
const LOGIN_PATH: &str = "/org/freedesktop/login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";
const SESSION: &str = "org.freedesktop.login1.Session";
/// The system bus when `DBUS_SYSTEM_BUS_ADDRESS` names none.
const SYSTEM_BUS: &str = "/run/dbus/system_bus_socket";
const MAX_FRAME: usize = 16 * 1024;
const MAX_LINE: usize = 512;
/// The D-Bus protocol's largest message; the bus sends none larger.
const MAX_MESSAGE: usize = 128 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(10);

/// What the host did that a notebook locks for.
#[derive(Debug)]
pub(super) enum Event {
    /// The session was locked through logind.
    Lock,
    /// The system is about to sleep; the inhibitor, when one is held,
    /// holds it until dropped.
    Suspend(Option<File>),
    /// The events can no longer be watched, and why.
    Lost(String),
}

/// The system bus socket: `DBUS_SYSTEM_BUS_ADDRESS` when it names one
/// absolute `unix:path=` address, else the standard path.
pub(super) fn system_bus(address: Option<&OsStr>) -> Result<PathBuf, String> {
    let Some(address) = address else {
        return Ok(PathBuf::from(SYSTEM_BUS));
    };
    address
        .to_str()
        .and_then(bus_client::unix_path)
        .map(PathBuf::from)
        .ok_or_else(|| "DBUS_SYSTEM_BUS_ADDRESS is not one absolute unix:path= address".to_owned())
}

/// Ends a watch when dropped: the connection is shut down, so the
/// watching thread reads its end, gives up its inhibitor and stops.
pub(super) struct Stop(pub(super) UnixStream);

impl Drop for Stop {
    fn drop(&mut self) {
        let _ = self.0.shutdown(Shutdown::Both);
    }
}

/// Watches this process's session over the system bus: events arrive on
/// the receiver from a thread of their own, which ends after reporting
/// the source lost or when the `Stop` is dropped. The flag says whether
/// logind granted a sleep delay when watching began. Connecting and
/// setting up can take up to twice the deadline.
pub(super) fn watch() -> Result<(Receiver<Event>, bool, Stop), String> {
    let path = system_bus(std::env::var_os("DBUS_SYSTEM_BUS_ADDRESS").as_deref())?;
    let shown = path.display().to_string();
    let stream =
        bus_client::connect_within(&path, DEADLINE, "host-bus-connect").map_err(|error| {
            if error.kind() == std::io::ErrorKind::TimedOut {
                format!("the system bus at {shown} did not answer")
            } else {
                format!("the system bus at {shown}: {error}")
            }
        })?;
    let uid = std::fs::metadata("/proc/self")
        .map_err(|error| format!("this process's account: {error}"))?
        .uid();
    let mut watcher = Watcher::start(stream, uid)?;
    let delays = watcher.inhibitor.is_some();
    let stop = Stop(
        watcher
            .stream
            .try_clone()
            .map_err(|error| format!("the system bus: {error}"))?,
    );
    let (events, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("host-events".to_owned())
        .spawn(move || loop {
            let event = watcher.next();
            let lost = matches!(event, Event::Lost(_));
            if events.send(event).is_err() || lost {
                break;
            }
        })
        .map_err(|error| format!("cannot watch host events: {error}"))?;
    Ok((receiver, delays, stop))
}

/// Descriptors received with one frame, closed unless taken.
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

impl Frame {
    fn message(&self) -> Result<Message<'_>, String> {
        message::decode(&self.bytes, self.fds.0.len() as u32)
            .map(|(message, _)| message)
            .map_err(|error| format!("the system bus sent {error}"))
    }
}

/// A system bus connection subscribed to logind's events for one session.
struct Watcher {
    stream: UnixStream,
    serial: u32,
    /// When the call under way must be answered; none while waiting for
    /// a signal.
    deadline: Option<Instant>,
    /// logind's unique name: only its signals count.
    owner: String,
    session: String,
    /// Watched signals read while awaiting a reply, in order.
    pending: VecDeque<Signal>,
    inhibitor: Option<File>,
}

impl Watcher {
    /// Authenticates as `uid`, finds the session this process belongs to,
    /// subscribes to its events and takes a sleep inhibitor where logind
    /// allows one.
    fn start(stream: UnixStream, uid: u32) -> Result<Self, String> {
        let mut watcher = Self {
            stream,
            serial: 0,
            deadline: Some(Instant::now() + DEADLINE),
            owner: String::new(),
            session: String::new(),
            pending: VecDeque::new(),
            inhibitor: None,
        };
        watcher.write(bus_client::auth_line(uid).as_bytes())?;
        if !watcher.line()?.starts_with("OK ") {
            return Err("the system bus refused this account".to_owned());
        }
        watcher.write(b"NEGOTIATE_UNIX_FD\r\n")?;
        if watcher.line()? != "AGREE_UNIX_FD" {
            return Err("the system bus passes no descriptors".to_owned());
        }
        watcher.write(b"BEGIN\r\n")?;
        watcher.call(BUS, BUS_PATH, BUS, "Hello", None)?;
        // Owner changes are watched before the owner is learned, so a
        // restart in between is seen.
        let owners = format!(
            "type='signal',sender='{BUS}',path='{BUS_PATH}',interface='{BUS}',\
             member='NameOwnerChanged',arg0='{LOGIN}'"
        );
        watcher.call(
            BUS,
            BUS_PATH,
            BUS,
            "AddMatch",
            Some(("s", &|w| w.string(&owners))),
        )?;
        let owner = watcher.call(
            BUS,
            BUS_PATH,
            BUS,
            "GetNameOwner",
            Some(("s", &|w| w.string(LOGIN))),
        )?;
        watcher.owner = one(&owner, "s")?;
        let session = watcher.call(
            LOGIN,
            LOGIN_PATH,
            MANAGER,
            "GetSessionByPID",
            // Zero asks logind for the caller's own session, by the bus's
            // credentials, so no PID namespace or reused pid misleads it.
            Some(("u", &|w: &mut Writer| {
                w.uint32(0);
                Ok(())
            })),
        )?;
        watcher.session = one(&session, "o")?;
        let rules = [
            format!(
                "type='signal',sender='{LOGIN}',path='{LOGIN_PATH}',\
                 interface='{MANAGER}',member='PrepareForSleep'"
            ),
            format!(
                "type='signal',sender='{LOGIN}',path='{}',interface='{SESSION}',member='Lock'",
                watcher.session
            ),
        ];
        for rule in &rules {
            watcher.call(
                BUS,
                BUS_PATH,
                BUS,
                "AddMatch",
                Some(("s", &|w| w.string(rule))),
            )?;
        }
        watcher.inhibit()?;
        watcher.deadline = None;
        Ok(watcher)
    }

    /// Takes a sleep inhibitor; logind refusing one leaves none held, and
    /// only the bus failing is an error.
    fn inhibit(&mut self) -> Result<(), String> {
        let fill = |w: &mut Writer| {
            w.string("sleep")?;
            w.string("td-pass")?;
            w.string("Lock the notebook before sleep")?;
            w.string("delay")
        };
        let reply = match self.call(LOGIN, LOGIN_PATH, MANAGER, "Inhibit", Some(("ssss", &fill))) {
            Ok(reply) => reply,
            Err(Refusal::Refused(_)) => return Ok(()),
            Err(Refusal::Failed(reason)) => return Err(reason),
        };
        let carried = {
            let message = reply.message()?;
            message.fields.signature == Some("h") && matches!(message.args(), [Value::UnixFd(0)])
        };
        // The guard closes whatever is not taken.
        let mut fds = reply.fds;
        let fd = match fds.0[..] {
            [_] if carried => fds.0.pop(),
            _ => None,
        }
        .ok_or("logind's inhibitor reply carried no descriptor")?;
        self.inhibitor = Some(sys::take_received(fd)?);
        Ok(())
    }

    /// The next event, waiting for it; failing to read the bus is the
    /// source lost.
    fn next(&mut self) -> Event {
        loop {
            let signal = match self.pending.pop_front() {
                Some(signal) => Some(signal),
                None => match self.frame() {
                    Ok(Some(frame)) => frame
                        .message()
                        .ok()
                        .and_then(|message| self.signal(&message)),
                    Ok(None) => None,
                    Err(reason) => return Event::Lost(reason),
                },
            };
            match signal {
                Some(Signal::Lock) => return Event::Lock,
                Some(Signal::Sleep(true)) => return Event::Suspend(self.inhibitor.take()),
                Some(Signal::Sleep(false)) => {
                    // Awake again: hold sleep for the next lock.
                    self.inhibitor = None;
                    self.deadline = Some(Instant::now() + DEADLINE);
                    let taken = self.inhibit();
                    self.deadline = None;
                    if let Err(reason) = taken {
                        return Event::Lost(reason);
                    }
                }
                Some(Signal::Lost) => {
                    return Event::Lost(
                        "logind stopped or restarted, so the host's lock and sleep are no \
                         longer watched"
                            .to_owned(),
                    )
                }
                None => {}
            }
        }
    }

    /// Which watched signal one message is, if it is one from its
    /// rightful sender.
    fn signal(&self, message: &Message<'_>) -> Option<Signal> {
        if message.kind != MessageType::Signal {
            return None;
        }
        let fields = &message.fields;
        let is = |sender: &str, path: &str, interface: &str, member: &str| {
            fields.sender == Some(sender)
                && fields.path == Some(path)
                && fields.interface == Some(interface)
                && fields.member == Some(member)
        };
        if is(&self.owner, LOGIN_PATH, MANAGER, "PrepareForSleep") {
            return match message.args() {
                [Value::Bool(sleeping)] => Some(Signal::Sleep(*sleeping)),
                _ => None,
            };
        }
        if is(&self.owner, &self.session, SESSION, "Lock") {
            return Some(Signal::Lock);
        }
        if is(BUS, BUS_PATH, BUS, "NameOwnerChanged") {
            return match message.args() {
                [Value::Str(name), ..] if *name == LOGIN => Some(Signal::Lost),
                _ => None,
            };
        }
        None
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        let remaining = self.remaining()?;
        self.stream
            .set_write_timeout(remaining)
            .and_then(|()| self.stream.write_all(bytes))
            .map_err(|error| format!("the system bus: {error}"))
    }

    /// What is left of the call's deadline; none while waiting for a
    /// signal.
    fn remaining(&self) -> Result<Option<Duration>, String> {
        match self.deadline {
            None => Ok(None),
            Some(deadline) => deadline
                .checked_duration_since(Instant::now())
                .filter(|left| !left.is_zero())
                .map(Some)
                .ok_or_else(|| "the system bus did not answer in time".to_owned()),
        }
    }

    /// Reads exactly `bytes`. The one descriptor a frame may carry is
    /// kept; more are closed at once, and the frame is spoiled.
    fn read(
        &self,
        mut bytes: &mut [u8],
        fds: &mut Descriptors,
        spoiled: &mut bool,
    ) -> Result<(), String> {
        while !bytes.is_empty() {
            self.stream
                .set_read_timeout(self.remaining()?)
                .map_err(|error| format!("the system bus: {error}"))?;
            let received =
                sys::recv_with_fds(&self.stream, bytes).map_err(|error| match error {
                    ReceiveError::Disconnected => "the system bus closed".to_owned(),
                    ReceiveError::TimedOut => "the system bus did not answer in time".to_owned(),
                    ReceiveError::Failure(reason) => format!("the system bus: {reason}"),
                })?;
            fds.0.extend(received.fds);
            if fds.0.len() > 1 {
                // The guard dropped closes them.
                *fds = Descriptors::default();
                *spoiled = true;
            }
            if received.count == 0 {
                return Err("the system bus closed".to_owned());
            }
            bytes = bytes
                .get_mut(received.count..)
                .ok_or("the system bus overran a read")?;
        }
        Ok(())
    }

    /// One authentication line, without its line end.
    fn line(&mut self) -> Result<String, String> {
        let mut line = Vec::new();
        let mut fds = Descriptors::default();
        let mut spoiled = false;
        loop {
            let mut byte = [0u8; 1];
            self.read(&mut byte, &mut fds, &mut spoiled)?;
            if byte == *b"\n" {
                break;
            }
            if line.len() >= MAX_LINE {
                return Err("the system bus sent an overlong line".to_owned());
            }
            line.extend_from_slice(&byte);
        }
        if spoiled || !fds.0.is_empty() {
            return Err("the system bus sent a descriptor while authenticating".to_owned());
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        String::from_utf8(line)
            .map_err(|_| "the system bus sent a line that is not text".to_owned())
    }

    /// The next frame and the descriptor that came with it, or none when
    /// one is passed over: larger than the bound, carrying more than one
    /// descriptor, or undecodable. Any account may send this connection a
    /// signal; only the bus's own framing is trusted.
    fn frame(&self) -> Result<Option<Frame>, String> {
        let mut head = [0u8; 16];
        let mut fds = Descriptors::default();
        let mut spoiled = false;
        self.read(&mut head, &mut fds, &mut spoiled)?;
        let total = frame_len(&head)?;
        if total > MAX_FRAME {
            let mut chunk = vec![0u8; MAX_FRAME];
            let mut left = total - 16;
            while left > 0 {
                let part = left.min(MAX_FRAME);
                let bytes = chunk.get_mut(..part).ok_or("a short chunk")?;
                self.read(bytes, &mut fds, &mut spoiled)?;
                // Closes what that part carried.
                fds = Descriptors::default();
                left -= part;
            }
            return Ok(None);
        }
        let mut bytes = head.to_vec();
        bytes.resize(total, 0);
        self.read(
            bytes.get_mut(16..).ok_or("a short frame")?,
            &mut fds,
            &mut spoiled,
        )?;
        if spoiled {
            return Ok(None);
        }
        let frame = Frame { bytes, fds };
        Ok(frame.message().is_ok().then_some(frame))
    }

    /// Calls `member` and returns its reply's frame: a return from the bus
    /// for the bus's calls and from the owner learned for logind's, or an
    /// error from either. Watched signals read while waiting are kept for
    /// `next`; every other frame is passed over, and the deadline bounds
    /// the wait.
    fn call(
        &mut self,
        destination: &str,
        path: &str,
        interface: &str,
        member: &str,
        body: Option<(&str, Fill<'_>)>,
    ) -> Result<Frame, Refusal> {
        self.serial = self
            .serial
            .checked_add(1)
            .ok_or("the bus serials ran out")?;
        let serial = self.serial;
        let mut builder = Builder::method_call(Endian::Little, path, Some(interface), member)
            .destination(destination)
            .serial(serial);
        if let Some((signature, fill)) = body {
            builder = builder
                .body(signature, fill)
                .map_err(|error| format!("{member}: {error}"))?;
        }
        let bytes = builder
            .encode()
            .map_err(|error| format!("{member}: {error}"))?;
        self.write(&bytes)?;
        let answerer = if destination == LOGIN {
            self.owner.clone()
        } else {
            BUS.to_owned()
        };
        loop {
            let Some(frame) = self.frame()? else {
                continue;
            };
            let step = {
                let message = frame.message()?;
                let sender = message.fields.sender;
                match message.kind {
                    MessageType::Signal => Step::Signal(self.signal(&message)),
                    MessageType::MethodReturn | MessageType::Error
                        if message.fields.reply_serial == Some(serial) =>
                    {
                        match message.fields.error_name {
                            None if sender == Some(&answerer) => Step::Answered,
                            Some(name) if sender == Some(&answerer) || sender == Some(BUS) => {
                                Step::Refused(name.to_owned())
                            }
                            _ => Step::Stranger,
                        }
                    }
                    _ => Step::Other,
                }
            };
            match step {
                Step::Answered => return Ok(frame),
                Step::Refused(name) => {
                    return Err(Refusal::Refused(format!("{member} was refused: {name}")))
                }
                Step::Stranger => {
                    return Err(Refusal::Failed(format!(
                        "{member} was answered by another than {destination}"
                    )))
                }
                Step::Signal(Some(signal)) => self.pending.push_back(signal),
                Step::Signal(None) | Step::Other => {}
            }
        }
    }
}

/// What one frame read while awaiting a reply is.
enum Step {
    Answered,
    Refused(String),
    /// A reply from neither the bus nor the callee.
    Stranger,
    Signal(Option<Signal>),
    Other,
}

/// Writes a call's arguments.
type Fill<'f> = &'f dyn Fn(&mut Writer) -> Result<(), WireError>;

/// A watched signal.
#[derive(Clone, Copy)]
enum Signal {
    Lock,
    Sleep(bool),
    Lost,
}

/// Why a call returned no reply: refused by its peer, or failed.
#[derive(Debug)]
enum Refusal {
    Refused(String),
    Failed(String),
}

impl From<String> for Refusal {
    fn from(reason: String) -> Self {
        Self::Failed(reason)
    }
}

impl From<&str> for Refusal {
    fn from(reason: &str) -> Self {
        Self::Failed(reason.to_owned())
    }
}

impl From<Refusal> for String {
    fn from(refusal: Refusal) -> Self {
        match refusal {
            Refusal::Refused(reason) | Refusal::Failed(reason) => reason,
        }
    }
}

/// A frame's whole length from its fixed header, without the codec's
/// ceilings: a frame past them is still read through and passed over.
/// The bus frames what it sends, so a length past the protocol's own
/// largest message, or a header it would not send, ends the watch.
fn frame_len(head: &[u8; 16]) -> Result<usize, String> {
    let word = |at: usize| -> Result<usize, String> {
        let bytes: [u8; 4] = head
            .get(at..at + 4)
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or("a short header")?;
        let value = match head.first() {
            Some(b'l') => u32::from_le_bytes(bytes),
            Some(b'B') => u32::from_be_bytes(bytes),
            _ => return Err("the system bus sent a frame of no known byte order".to_owned()),
        };
        usize::try_from(value).map_err(|_| "a frame too large".to_owned())
    };
    let fields = word(12)?;
    let body = word(4)?;
    16usize
        .checked_add(fields)
        .and_then(|end| end.checked_next_multiple_of(8))
        .and_then(|end| end.checked_add(body))
        .filter(|total| *total <= MAX_MESSAGE)
        .ok_or_else(|| "the system bus sent a frame past the protocol's bound".to_owned())
}

/// The one string or object path a reply carries.
fn one(reply: &Frame, signature: &str) -> Result<String, String> {
    let message = reply.message()?;
    match message.args() {
        [Value::Str(text)] | [Value::ObjectPath(text)]
            if message.fields.signature == Some(signature) =>
        {
            Ok((*text).to_owned())
        }
        _ => Err(format!("a reply carried no {signature}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::thread::JoinHandle;

    const OWNER: &str = ":1.2";
    const OURS: &str = "/org/freedesktop/login1/session/c1";

    /// The bus's end of a scripted connection.
    struct Bus {
        stream: UnixStream,
        serial: u32,
    }

    /// A method call the watcher made: its member and its arguments.
    #[derive(Debug, PartialEq)]
    struct Call {
        serial: u32,
        destination: String,
        member: String,
        args: Vec<String>,
    }

    impl Bus {
        fn line(&mut self) -> String {
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                self.stream.read_exact(&mut byte).unwrap();
                if byte == *b"\n" {
                    return String::from_utf8(line).unwrap();
                }
                line.push(byte[0]);
            }
        }

        fn send(&mut self, bytes: &[u8]) {
            self.stream.write_all(bytes).unwrap();
        }

        fn serial(&mut self) -> u32 {
            self.serial += 1;
            self.serial
        }

        fn call(&mut self) -> Call {
            let mut head = [0u8; 16];
            self.stream.read_exact(&mut head).unwrap();
            let mut bytes = head.to_vec();
            bytes.resize(message::frame_len(&head).unwrap().unwrap(), 0);
            self.stream.read_exact(&mut bytes[16..]).unwrap();
            let (message, _) = message::decode(&bytes, 0).unwrap();
            assert_eq!(message.kind, MessageType::MethodCall);
            Call {
                serial: message.serial,
                destination: message.fields.destination.unwrap().to_owned(),
                member: message.fields.member.unwrap().to_owned(),
                args: message
                    .args()
                    .iter()
                    .map(|value| match value {
                        Value::Uint32(number) => number.to_string(),
                        value => value.as_str().unwrap().to_owned(),
                    })
                    .collect(),
            }
        }

        fn reply(&mut self, to: &Call, signature: &str, text: &str) {
            let sender = if to.destination == LOGIN { OWNER } else { BUS };
            self.reply_from(sender, to, signature, text);
        }

        fn reply_from(&mut self, sender: &str, to: &Call, signature: &str, text: &str) {
            let serial = self.serial();
            let builder = Builder::method_return(Endian::Little, to.serial)
                .serial(serial)
                .sender(sender);
            let builder = match signature {
                "" => builder,
                "o" => builder.body("o", |w| w.object_path(text)).unwrap(),
                _ => builder.body("s", |w| w.string(text)).unwrap(),
            };
            self.send(&builder.encode().unwrap());
        }

        fn refuse(&mut self, to: &Call, name: &str) {
            let serial = self.serial();
            let bytes = Builder::error(Endian::Little, name, to.serial)
                .serial(serial)
                .sender(OWNER)
                .encode()
                .unwrap();
            self.send(&bytes);
        }

        /// Answers an Inhibit call with a descriptor; the peer returned
        /// reads end of file once every copy of it is closed.
        fn grant(&mut self, to: &Call) -> UnixStream {
            let (delay, peer) = UnixStream::pair().unwrap();
            let serial = self.serial();
            let bytes = Builder::method_return(Endian::Little, to.serial)
                .serial(serial)
                .sender(OWNER)
                .unix_fds(1)
                .body("h", |w| {
                    w.unix_fd(0);
                    Ok(())
                })
                .unwrap()
                .encode()
                .unwrap();
            sys::send_with_fd(&self.stream, &bytes, delay.as_raw_fd()).unwrap();
            peer
        }

        fn signal(&mut self, sender: &str, path: &str, interface: &str, member: &str, arg: Arg) {
            let serial = self.serial();
            let builder = Builder::signal(Endian::Little, path, interface, member)
                .serial(serial)
                .sender(sender);
            let builder = match arg {
                Arg::None => builder,
                Arg::Sleep(sleeping) => builder
                    .body("b", |w| {
                        w.bool(sleeping);
                        Ok(())
                    })
                    .unwrap(),
                Arg::Owner(name) => builder
                    .body("sss", |w| {
                        w.string(name)?;
                        w.string(OWNER)?;
                        w.string("")
                    })
                    .unwrap(),
            };
            self.send(&builder.encode().unwrap());
        }

        fn sleep(&mut self, sleeping: bool) {
            self.signal(
                OWNER,
                LOGIN_PATH,
                MANAGER,
                "PrepareForSleep",
                Arg::Sleep(sleeping),
            );
        }

        fn lock(&mut self, sender: &str, path: &str) {
            self.signal(sender, path, SESSION, "Lock", Arg::None);
        }

        /// Answers authentication and setup; Inhibit is left to `inhibit`.
        fn setup(&mut self) -> Vec<Call> {
            assert_eq!(self.line(), "\0AUTH EXTERNAL 31303030\r");
            self.send(b"OK 0123456789abcdef0123456789abcdef\r\n");
            assert_eq!(self.line(), "NEGOTIATE_UNIX_FD\r");
            self.send(b"AGREE_UNIX_FD\r\n");
            assert_eq!(self.line(), "BEGIN\r");
            let mut calls = Vec::new();
            for answer in [":1.7", "", OWNER, OURS, "", ""] {
                let call = self.call();
                let signature = match call.member.as_str() {
                    "GetSessionByPID" => "o",
                    "AddMatch" => "",
                    _ => "s",
                };
                self.reply(&call, signature, answer);
                calls.push(call);
            }
            calls
        }
    }

    enum Arg {
        None,
        Sleep(bool),
        Owner(&'static str),
    }

    fn scripted<T: Send + 'static>(
        script: impl FnOnce(&mut Bus) -> T + Send + 'static,
    ) -> (Result<Watcher, String>, JoinHandle<T>) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let bus = std::thread::spawn(move || {
            script(&mut Bus {
                stream: theirs,
                serial: 0,
            })
        });
        (Watcher::start(ours, 1000), bus)
    }

    fn held(peer: &UnixStream) -> bool {
        peer.set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        match (&*peer).read(&mut [0u8; 1]) {
            Ok(0) => false,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => true,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn setup_finds_the_session_and_subscribes_to_its_events() {
        let (watcher, bus) = scripted(|bus| {
            let mut calls = bus.setup();
            let inhibit = bus.call();
            let peer = bus.grant(&inhibit);
            calls.push(inhibit);
            (calls, peer)
        });
        let watcher = watcher.unwrap();
        let (calls, peer) = bus.join().unwrap();
        let called: Vec<(&str, &str, Vec<&str>)> = calls
            .iter()
            .map(|call| {
                (
                    call.destination.as_str(),
                    call.member.as_str(),
                    call.args.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            called,
            [
                (BUS, "Hello", vec![]),
                (
                    BUS,
                    "AddMatch",
                    vec![
                        "type='signal',sender='org.freedesktop.DBus',\
                         path='/org/freedesktop/DBus',interface='org.freedesktop.DBus',\
                         member='NameOwnerChanged',arg0='org.freedesktop.login1'"
                    ]
                ),
                (BUS, "GetNameOwner", vec![LOGIN]),
                (LOGIN, "GetSessionByPID", vec!["0"]),
                (
                    BUS,
                    "AddMatch",
                    vec![
                        "type='signal',sender='org.freedesktop.login1',\
                         path='/org/freedesktop/login1',\
                         interface='org.freedesktop.login1.Manager',member='PrepareForSleep'"
                    ]
                ),
                (
                    BUS,
                    "AddMatch",
                    vec![
                        "type='signal',sender='org.freedesktop.login1',\
                         path='/org/freedesktop/login1/session/c1',\
                         interface='org.freedesktop.login1.Session',member='Lock'"
                    ]
                ),
                (
                    LOGIN,
                    "Inhibit",
                    vec![
                        "sleep",
                        "td-pass",
                        "Lock the notebook before sleep",
                        "delay"
                    ]
                ),
            ]
        );
        assert_eq!(watcher.owner, OWNER);
        assert_eq!(watcher.session, OURS);
        assert!(watcher.deadline.is_none());
        assert!(held(&peer));
        drop(watcher);
        assert!(!held(&peer));
    }

    #[test]
    fn only_logind_s_own_signals_for_this_session_count() {
        let (watcher, bus) = scripted(|bus| {
            bus.setup();
            let inhibit = bus.call();
            let first = bus.grant(&inhibit);
            // Another sender's lock and another session's are not this
            // session's: the next event is the sleep.
            bus.lock(":1.9", OURS);
            bus.lock(OWNER, "/org/freedesktop/login1/session/c2");
            bus.sleep(true);
            // Another sender's sleep, another name's owner change and one
            // forged by logind are not events: the next is the lock.
            bus.signal(
                ":1.9",
                LOGIN_PATH,
                MANAGER,
                "PrepareForSleep",
                Arg::Sleep(true),
            );
            bus.signal(
                BUS,
                BUS_PATH,
                BUS,
                "NameOwnerChanged",
                Arg::Owner("org.example"),
            );
            bus.signal(OWNER, BUS_PATH, BUS, "NameOwnerChanged", Arg::Owner(LOGIN));
            bus.lock(OWNER, OURS);
            // Awake: the watcher takes a new delay. Signals of no interest
            // while it waits pass, and a lock is kept.
            bus.sleep(false);
            let inhibit = bus.call();
            for _ in 0..64 {
                bus.signal(":1.9", "/", "org.example.Noise", "Noise", Arg::None);
            }
            bus.lock(OWNER, OURS);
            let second = bus.grant(&inhibit);
            bus.sleep(true);
            bus.signal(BUS, BUS_PATH, BUS, "NameOwnerChanged", Arg::Owner(LOGIN));
            (first, second)
        });
        let mut watcher = watcher.unwrap();
        let Event::Suspend(Some(delay)) = watcher.next() else {
            panic!("no delay");
        };
        assert!(matches!(watcher.next(), Event::Lock));
        assert!(matches!(watcher.next(), Event::Lock));
        let (first, second) = bus.join().unwrap();
        assert!(held(&first));
        drop(delay);
        assert!(!held(&first));
        let Event::Suspend(Some(delay)) = watcher.next() else {
            panic!("no second delay");
        };
        assert!(held(&second));
        drop(delay);
        assert!(!held(&second));
        let Event::Lost(reason) = watcher.next() else {
            panic!("logind's restart was not seen");
        };
        assert!(reason.contains("logind stopped or restarted"), "{reason}");
    }

    #[test]
    fn frames_another_account_sends_are_passed_over_and_their_descriptors_closed() {
        let (watcher, bus) = scripted(|bus| {
            bus.setup();
            let inhibit = bus.call();
            bus.refuse(&inhibit, "org.freedesktop.DBus.Error.AccessDenied");
            // A descriptor on another's signal.
            let (stray, stray_peer) = UnixStream::pair().unwrap();
            let serial = bus.serial();
            let bytes = Builder::signal(Endian::Little, "/", "org.example.Noise", "Noise")
                .serial(serial)
                .sender(":1.9")
                .unix_fds(1)
                .encode()
                .unwrap();
            sys::send_with_fd(&bus.stream, &bytes, stray.as_raw_fd()).unwrap();
            // Two descriptors spoil a frame, even a lock from logind.
            let (one, one_peer) = UnixStream::pair().unwrap();
            let (two, two_peer) = UnixStream::pair().unwrap();
            let serial = bus.serial();
            let bytes = Builder::signal(Endian::Little, OURS, SESSION, "Lock")
                .serial(serial)
                .sender(OWNER)
                .encode()
                .unwrap();
            sys::send_with_fd(&bus.stream, &bytes[..8], one.as_raw_fd()).unwrap();
            sys::send_with_fd(&bus.stream, &bytes[8..], two.as_raw_fd()).unwrap();
            // A frame past the bound.
            let serial = bus.serial();
            let long = "x".repeat(2 * MAX_FRAME);
            let bytes = Builder::signal(Endian::Little, "/", "org.example.Noise", "Noise")
                .serial(serial)
                .sender(":1.9")
                .body("s", |w| w.string(&long))
                .unwrap()
                .encode()
                .unwrap();
            bus.send(&bytes);
            bus.sleep(true);
            [stray_peer, one_peer, two_peer]
        });
        let mut watcher = watcher.unwrap();
        assert!(matches!(watcher.next(), Event::Suspend(None)));
        for peer in bus.join().unwrap() {
            assert!(!held(&peer));
        }
    }

    #[test]
    fn a_restart_while_setting_up_is_seen() {
        let (watcher, bus) = scripted(|bus| {
            bus.line();
            bus.send(b"OK 0123456789abcdef0123456789abcdef\r\n");
            bus.line();
            bus.send(b"AGREE_UNIX_FD\r\n");
            bus.line();
            for answer in [":1.7", "", OWNER] {
                let call = bus.call();
                let signature = if answer.is_empty() { "" } else { "s" };
                bus.reply(&call, signature, answer);
            }
            // logind goes away once its name is learned.
            bus.signal(BUS, BUS_PATH, BUS, "NameOwnerChanged", Arg::Owner(LOGIN));
            for answer in [OURS, "", ""] {
                let call = bus.call();
                let signature = if answer.is_empty() { "" } else { "o" };
                bus.reply(&call, signature, answer);
            }
            let inhibit = bus.call();
            bus.grant(&inhibit)
        });
        let mut watcher = watcher.unwrap();
        let peer = bus.join().unwrap();
        let Event::Lost(reason) = watcher.next() else {
            panic!("the restart was not seen");
        };
        assert!(reason.contains("logind stopped or restarted"), "{reason}");
        drop(watcher);
        assert!(!held(&peer));
    }

    #[test]
    fn a_stopped_watch_ends_and_gives_up_its_delay() {
        let (watcher, bus) = scripted(|bus| {
            bus.setup();
            let inhibit = bus.call();
            let peer = bus.grant(&inhibit);
            (
                peer,
                std::mem::replace(&mut bus.stream, UnixStream::pair().unwrap().0),
            )
        });
        let mut watcher = watcher.unwrap();
        // The bus's end stays open: only the stop ends the watch.
        let (peer, _open) = bus.join().unwrap();
        let stop = Stop(watcher.stream.try_clone().unwrap());
        let watching = std::thread::spawn(move || {
            let event = watcher.next();
            drop(watcher);
            event
        });
        assert!(held(&peer));
        drop(stop);
        assert!(matches!(watching.join().unwrap(), Event::Lost(_)));
        assert!(!held(&peer));
    }

    #[test]
    fn a_refused_inhibitor_leaves_sleep_undelayed() {
        let (watcher, bus) = scripted(|bus| {
            bus.setup();
            let inhibit = bus.call();
            bus.refuse(&inhibit, "org.freedesktop.DBus.Error.AccessDenied");
            bus.sleep(true);
            bus.sleep(false);
            let inhibit = bus.call();
            bus.refuse(&inhibit, "org.freedesktop.DBus.Error.AccessDenied");
            bus.sleep(true);
        });
        let mut watcher = watcher.unwrap();
        assert!(watcher.inhibitor.is_none());
        assert!(matches!(watcher.next(), Event::Suspend(None)));
        assert!(matches!(watcher.next(), Event::Suspend(None)));
        bus.join().unwrap();
        let Event::Lost(reason) = watcher.next() else {
            panic!("the closed bus was not seen");
        };
        assert!(reason.contains("closed"), "{reason}");
    }

    #[test]
    fn setup_refuses_what_it_cannot_watch() {
        let refused = |script: fn(&mut Bus)| {
            let (watcher, bus) = scripted(script);
            let reason = watcher.err().unwrap();
            bus.join().unwrap();
            reason
        };
        let reason = refused(|bus| {
            bus.line();
            bus.send(b"REJECTED EXTERNAL\r\n");
        });
        assert_eq!(reason, "the system bus refused this account");
        let reason = refused(|bus| {
            bus.line();
            bus.send(b"OK 0123456789abcdef0123456789abcdef\r\n");
            bus.line();
            bus.send(b"ERROR\r\n");
        });
        assert_eq!(reason, "the system bus passes no descriptors");
        let reason = refused(|bus| {
            bus.line();
            bus.send(b"OK 0123456789abcdef0123456789abcdef\r\n");
            bus.line();
            bus.send(b"AGREE_UNIX_FD\r\n");
            bus.line();
            for answer in [":1.7", "", OWNER] {
                let call = bus.call();
                let signature = if answer.is_empty() { "" } else { "s" };
                bus.reply(&call, signature, answer);
            }
            let call = bus.call();
            bus.refuse(&call, "org.freedesktop.login1.NoSessionForPID");
        });
        assert_eq!(
            reason,
            "GetSessionByPID was refused: org.freedesktop.login1.NoSessionForPID"
        );
        let reason = refused(|bus| {
            bus.line();
            bus.send(b"OK 0123456789abcdef0123456789abcdef\r\n");
            bus.line();
            bus.send(b"AGREE_UNIX_FD\r\n");
            bus.line();
            let call = bus.call();
            bus.reply(&call, "s", ":1.7");
            let call = bus.call();
            bus.reply(&call, "", "");
            let call = bus.call();
            // A path where a name belongs.
            bus.reply(&call, "o", "/");
        });
        assert_eq!(reason, "a reply carried no s");
        let reason = refused(|bus| {
            bus.line();
            bus.send(b"OK 0123456789abcdef0123456789abcdef\r\n");
            bus.line();
            bus.send(b"AGREE_UNIX_FD\r\n");
            bus.line();
            for answer in [":1.7", "", OWNER] {
                let call = bus.call();
                let signature = if answer.is_empty() { "" } else { "s" };
                bus.reply(&call, signature, answer);
            }
            let call = bus.call();
            bus.reply_from(":1.9", &call, "o", OURS);
        });
        assert_eq!(
            reason,
            "GetSessionByPID was answered by another than org.freedesktop.login1"
        );
    }

    #[test]
    fn an_inhibitor_reply_must_name_its_descriptor() {
        let (watcher, bus) = scripted(|bus| {
            bus.setup();
            let inhibit = bus.call();
            let (delay, peer) = UnixStream::pair().unwrap();
            let serial = bus.serial();
            let bytes = Builder::method_return(Endian::Little, inhibit.serial)
                .serial(serial)
                .sender(OWNER)
                .unix_fds(1)
                .body("s", |w| w.string("not a descriptor"))
                .unwrap()
                .encode()
                .unwrap();
            sys::send_with_fd(&bus.stream, &bytes, delay.as_raw_fd()).unwrap();
            peer
        });
        let reason = watcher.err().unwrap();
        assert_eq!(reason, "logind's inhibitor reply carried no descriptor");
        assert!(!held(&bus.join().unwrap()));
    }

    #[test]
    fn a_call_passes_over_other_frames_until_its_reply() {
        let (watcher, bus) = scripted(|bus| {
            bus.line();
            bus.send(b"OK 0123456789abcdef0123456789abcdef\r\n");
            bus.line();
            bus.send(b"AGREE_UNIX_FD\r\n");
            bus.line();
            for _ in 0..2 {
                let call = bus.call();
                bus.reply(&call, "", "");
            }
            let call = bus.call();
            for _ in 0..64 {
                let serial = bus.serial();
                let bytes = Builder::method_call(Endian::Little, "/", Some("org.example"), "Ask")
                    .serial(serial)
                    .sender(":1.9")
                    .encode()
                    .unwrap();
                bus.send(&bytes);
            }
            // A frame past the codec's ceilings: header fields over 64 KiB
            // and a body over 16 MiB, read through.
            for (fields, body) in [(80 * 1024u32, 0u32), (0, 17 * 1024 * 1024)] {
                let mut head = vec![b'l', 4, 0, 1];
                head.extend_from_slice(&body.to_le_bytes());
                head.extend_from_slice(&1u32.to_le_bytes());
                head.extend_from_slice(&fields.to_le_bytes());
                bus.send(&head);
                let padded = (fields as usize).next_multiple_of(8) + body as usize;
                bus.send(&vec![0u8; padded]);
            }
            bus.reply(&call, "s", OWNER);
            let call = bus.call();
            bus.refuse(&call, "org.freedesktop.login1.NoSessionForPID");
        });
        assert_eq!(
            watcher.err().unwrap(),
            "GetSessionByPID was refused: org.freedesktop.login1.NoSessionForPID"
        );
        bus.join().unwrap();
    }

    #[test]
    fn a_frame_past_the_protocol_s_bound_ends_the_watch() {
        let mut head = [0u8; 16];
        head[0] = b'l';
        head[4..8].copy_from_slice(&(MAX_MESSAGE as u32).to_le_bytes());
        assert!(frame_len(&head).is_err());
        head[4..8].copy_from_slice(&8u32.to_le_bytes());
        head[12..16].copy_from_slice(&3u32.to_le_bytes());
        assert_eq!(frame_len(&head), Ok(32));
        head[0] = b'B';
        assert!(frame_len(&head).is_err());
        head[4..8].copy_from_slice(&8u32.to_be_bytes());
        head[12..16].copy_from_slice(&3u32.to_be_bytes());
        assert_eq!(frame_len(&head), Ok(32));
        head[0] = b'x';
        assert!(frame_len(&head).is_err());
    }

    #[test]
    fn the_system_bus_is_one_absolute_unix_path() {
        assert_eq!(
            system_bus(None).unwrap(),
            PathBuf::from("/run/dbus/system_bus_socket")
        );
        assert_eq!(
            system_bus(Some(OsStr::new(
                "unix:path=/var/run/dbus/system_bus_socket"
            )))
            .unwrap(),
            PathBuf::from("/var/run/dbus/system_bus_socket")
        );
        for refused in [
            "",
            "unix:path=relative",
            "unix:abstract=/tmp/bus",
            "tcp:host=localhost,port=1",
            "unix:path=/a;unix:path=/b",
            "unix:path=/run/dbus/system_bus_socket,guid=00",
            "unix:path=/run/%64bus",
        ] {
            assert!(system_bus(Some(OsStr::new(refused))).is_err(), "{refused}");
        }
    }

    #[test]
    fn descriptors_are_received_and_taken_only_here() {
        let source = include_str!("portable_events.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        for site in [
            "sys::recv_with_fds(",
            "sys::take_received(",
            "sys::discard_received(",
        ] {
            assert_eq!(source.matches(site).count(), 1, "{site}");
        }
        assert!(!source.contains("from_raw_fd"));
        assert!(!source.contains("send_with_fd"));
        // Only a reply to Inhibit gives up a descriptor.
        assert_eq!(source.matches("fds.0.pop()").count(), 1);
    }
}
