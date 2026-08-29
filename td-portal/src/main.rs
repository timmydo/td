#![deny(unsafe_code)]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

//! td-portal — the supervised desktop portal service.
//!
//! The first landing serves the synchronous Settings interface. A root
//! supervisor obtains td-busd's one-shot ownership capability, starts its
//! direct unprivileged child through `td-login exec-as`, and stays alive while
//! that child owns `org.freedesktop.portal.Desktop`.

#[path = "../../td-busd/src/message.rs"]
#[allow(
    dead_code,
    reason = "the shared broker codec is broader than one portal"
)]
mod message;
#[path = "../../td-busd/src/name.rs"]
mod name;
mod settings;
#[path = "../../td-busd/src/wire.rs"]
#[allow(
    dead_code,
    reason = "the shared broker codec is broader than one portal"
)]
mod wire;

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use message::{Message, MessageType};
use settings::{Settings, APPEARANCE, GNOME_INTERFACE, NAMESPACE_COUNT};
use wire::{Endian, Limits, Value, WireError, Writer};

const BUS_NAME: &str = "org.freedesktop.DBus";
const BUS_PATH: &str = "/org/freedesktop/DBus";
const PORTAL_ACTIVATION_INTERFACE: &str = "td.Portal1";
const PORTAL_ACTIVATION_PATH: &str = "/td/Portal1";
const PORTAL_NAME: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SETTINGS_INTERFACE: &str = "org.freedesktop.portal.Settings";
const PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";
const INTROSPECT_INTERFACE: &str = "org.freedesktop.DBus.Introspectable";
const PEER_INTERFACE: &str = "org.freedesktop.DBus.Peer";
const PORTAL_VERSION: u32 = 1;
const REQUEST_NAME_DO_NOT_QUEUE: u32 = 4;
const REQUEST_NAME_PRIMARY_OWNER: u32 = 1;
const UI_UID: u32 = 1000;
const PORTAL_LOGIN: &str = "/bin/td-login";
const PORTAL_PROGRAM: &str = "/bin/td-portal";
const PORTAL_USER: &str = "tester";
const ACTIVATION_TOKEN_BYTES: usize = 32;
const MAX_PORTAL_FRAME: usize = 256 * 1024;
const MAX_UNRELATED_MESSAGES: usize = 16;
const MAX_NAMESPACE_FILTERS: usize = 32;
const MAX_NAMESPACE_BYTES: usize = 255;
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(20);
pub const READY_MARKER: &str = "TD-PORTAL-READY namespaces=2 settings=10 version=1";

const INTROSPECTION_XML: &str = r#"<node>
  <interface name="org.freedesktop.portal.Settings">
    <method name="ReadAll">
      <arg type="as" name="namespaces" direction="in"/>
      <arg type="a{sa{sv}}" name="value" direction="out"/>
    </method>
    <method name="Read">
      <arg type="s" name="namespace" direction="in"/>
      <arg type="s" name="key" direction="in"/>
      <arg type="v" name="value" direction="out"/>
    </method>
    <property name="version" type="u" access="read"/>
  </interface>
  <interface name="org.freedesktop.DBus.Properties">
    <method name="Get"><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="v" direction="out"/></method>
    <method name="GetAll"><arg type="s" direction="in"/><arg type="a{sv}" direction="out"/></method>
    <method name="Set"><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="v" direction="in"/></method>
    <signal name="PropertiesChanged"><arg type="s"/><arg type="a{sv}"/><arg type="as"/></signal>
  </interface>
  <interface name="org.freedesktop.DBus.Introspectable">
    <method name="Introspect"><arg type="s" direction="out"/></method>
  </interface>
  <interface name="org.freedesktop.DBus.Peer">
    <method name="Ping"/>
  </interface>
</node>"#;

fn usage() -> String {
    "usage: td-portal supervise --bus PATH --settings PATH | \
     td-portal run --bus PATH --settings PATH --activation-token TOKEN | \
     td-portal probe --bus PATH --settings PATH | td-portal selftest"
        .into()
}

#[derive(Debug, Eq, PartialEq)]
struct Paths {
    bus: PathBuf,
    settings: PathBuf,
    token: Option<String>,
}

fn parse_paths(args: &[String], token_required: bool) -> Result<Paths, String> {
    let mut bus = None;
    let mut settings = None;
    let mut token = None;
    let mut at = 0usize;
    while at < args.len() {
        let flag = args
            .get(at)
            .ok_or_else(|| "a portal flag went missing".to_string())?;
        let value = args
            .get(at.saturating_add(1))
            .ok_or_else(|| format!("{flag} requires a value"))?;
        match flag.as_str() {
            "--bus" if bus.is_none() => bus = Some(PathBuf::from(value)),
            "--settings" if settings.is_none() => settings = Some(PathBuf::from(value)),
            "--activation-token" if token.is_none() => token = Some(value.clone()),
            "--bus" | "--settings" | "--activation-token" => {
                return Err(format!("duplicate portal flag {flag}"))
            }
            _ => return Err(format!("unrecognised portal flag {flag:?}")),
        }
        at = at.saturating_add(2);
    }
    let bus = bus.ok_or_else(|| "--bus is required".to_string())?;
    let settings = settings.ok_or_else(|| "--settings is required".to_string())?;
    if !bus.is_absolute() || !settings.is_absolute() {
        return Err("portal bus and settings paths must be absolute".into());
    }
    match (token_required, token.as_deref()) {
        (true, Some(value)) if valid_token(value) => {}
        (true, Some(_)) => return Err("the activation token is not 32 lowercase hex bytes".into()),
        (true, None) => return Err("--activation-token is required".into()),
        (false, Some(_)) => return Err("--activation-token is valid only for run".into()),
        (false, None) => {}
    }
    Ok(Paths {
        bus,
        settings,
        token,
    })
}

fn valid_token(token: &str) -> bool {
    token.len() == ACTIVATION_TOKEN_BYTES
        && token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn current_uid() -> Result<u32, String> {
    fs::metadata("/proc/self")
        .map(|metadata| metadata.uid())
        .map_err(|error| format!("cannot identify td-portal through /proc/self: {error}"))
}

struct Timed<'a> {
    stream: &'a UnixStream,
    until: Instant,
}

impl Timed<'_> {
    fn left(&self) -> io::Result<Duration> {
        self.until
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "the D-Bus exchange timed out"))
    }
}

impl Read for Timed<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.stream.set_read_timeout(Some(self.left()?))?;
        (&*self.stream).read(buffer)
    }
}

impl Write for Timed<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.stream.set_write_timeout(Some(self.left()?))?;
        (&*self.stream).write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&*self.stream).flush()
    }
}

struct Connection {
    stream: UnixStream,
    serial: u32,
    unique: Option<String>,
    until: Option<Instant>,
}

#[derive(Debug)]
struct Reply {
    endian: Endian,
    signature: String,
    body: Vec<u8>,
}

enum IncomingFrame {
    Message(Vec<u8>),
    Oversized {
        total: usize,
        call: Option<OversizedCall>,
    },
}

#[derive(Debug, Eq, PartialEq)]
struct OversizedCall {
    endian: Endian,
    serial: u32,
    sender: String,
}

impl Reply {
    fn values(&self) -> io::Result<Vec<Value<'_>>> {
        wire::read_body(&self.body, &self.signature, self.endian, Limits::NO_FDS)
            .map_err(wire_error)
    }

    fn no_body(&self) -> io::Result<()> {
        if self.signature.is_empty() && self.body.is_empty() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "expected an empty reply, got signature {:?} and {} body bytes",
                self.signature,
                self.body.len()
            )))
        }
    }

    fn one_string(&self) -> io::Result<String> {
        if self.signature != "s" {
            return Err(io::Error::other(format!(
                "expected a string reply, got {:?}",
                self.signature
            )));
        }
        let values = self.values()?;
        match values.as_slice() {
            [Value::Str(text)] => Ok((*text).to_string()),
            _ => Err(io::Error::other("the string reply body has another shape")),
        }
    }

    fn one_u32(&self) -> io::Result<u32> {
        if self.signature != "u" {
            return Err(io::Error::other(format!(
                "expected a uint32 reply, got {:?}",
                self.signature
            )));
        }
        let values = self.values()?;
        match values.as_slice() {
            [Value::Uint32(value)] => Ok(*value),
            _ => Err(io::Error::other("the uint32 reply body has another shape")),
        }
    }
}

impl Connection {
    fn open(socket: &Path, uid: u32) -> io::Result<Self> {
        let until = Instant::now()
            .checked_add(EXCHANGE_TIMEOUT)
            .ok_or_else(|| io::Error::other("the D-Bus deadline is not representable"))?;
        let stream = connect_within(socket, EXCHANGE_TIMEOUT).map_err(|error| {
            io::Error::other(format!(
                "connect to session bus {}: {error}",
                socket.display()
            ))
        })?;
        let mut connection = Self {
            stream,
            serial: 0,
            unique: None,
            until: Some(until),
        };
        connection.handshake(uid)?;
        let hello = connection.call(BUS_NAME, BUS_PATH, BUS_NAME, "Hello", "", |_| Ok(()))?;
        let unique = hello.one_string()?;
        if !name::valid_unique_name(&unique) {
            return Err(io::Error::other("Hello returned a malformed unique name"));
        }
        connection.unique = Some(unique);
        Ok(connection)
    }

    fn timed(&self) -> io::Result<Timed<'_>> {
        let until = self
            .until
            .ok_or_else(|| io::Error::other("the setup deadline is no longer active"))?;
        Ok(Timed {
            stream: &self.stream,
            until,
        })
    }

    fn handshake(&mut self, uid: u32) -> io::Result<()> {
        self.timed()?.write_all(auth_line(uid).as_bytes())?;
        let line = self.read_line()?;
        if !line.starts_with("OK ") {
            return Err(io::Error::other(format!(
                "the session bus refused this identity: {}",
                line.trim()
            )));
        }
        self.timed()?.write_all(b"BEGIN\r\n")
    }

    fn read_line(&mut self) -> io::Result<String> {
        let mut line = Vec::new();
        loop {
            let mut one = [0u8; 1];
            self.timed()?.read_exact(&mut one)?;
            let [byte] = one;
            if byte == b'\n' {
                break;
            }
            if line.len() >= 512 {
                return Err(io::Error::other("the bus sent an oversized handshake line"));
            }
            line.push(byte);
        }
        String::from_utf8(line)
            .map_err(|_| io::Error::other("the bus sent a non-UTF-8 handshake line"))
    }

    fn next_serial(&mut self) -> io::Result<u32> {
        self.serial = self
            .serial
            .checked_add(1)
            .ok_or_else(|| io::Error::other("this D-Bus connection ran out of serials"))?;
        Ok(self.serial)
    }

    fn call<F>(
        &mut self,
        destination: &str,
        path: &str,
        interface: &str,
        member: &str,
        signature: &str,
        fill: F,
    ) -> io::Result<Reply>
    where
        F: FnOnce(&mut Writer) -> Result<(), WireError>,
    {
        let serial = self.next_serial()?;
        let frame = message::Builder::method_call(Endian::Little, path, Some(interface), member)
            .destination(destination)
            .serial(serial)
            .body(signature, fill)
            .map_err(wire_error)?
            .encode()
            .map_err(message_error)?;
        self.timed()?.write_all(&frame)?;
        for _ in 0..=MAX_UNRELATED_MESSAGES {
            let bytes = match read_frame(&mut self.timed()?)? {
                IncomingFrame::Message(bytes) => bytes,
                IncomingFrame::Oversized { total, .. } => {
                    return Err(io::Error::other(format!(
                        "the D-Bus reply is {total} bytes, over the {MAX_PORTAL_FRAME}-byte portal ceiling"
                    )))
                }
            };
            let (reply, consumed) = message::decode(&bytes, 0).map_err(message_error)?;
            if consumed != bytes.len() {
                return Err(io::Error::other("a D-Bus frame carried trailing bytes"));
            }
            if reply.fields.reply_serial != Some(serial) {
                continue;
            }
            let sender = reply
                .fields
                .sender
                .ok_or_else(|| io::Error::other("a D-Bus reply has no authenticated sender"))?;
            if destination == BUS_NAME && sender != BUS_NAME {
                continue;
            }
            if let Some(unique) = &self.unique {
                if reply.fields.destination != Some(unique.as_str()) {
                    continue;
                }
            }
            if reply.kind == MessageType::Error {
                let text = reply.args().first().and_then(Value::as_str).unwrap_or("");
                return Err(io::Error::other(format!(
                    "{member} was refused as {}{}",
                    reply.fields.error_name.unwrap_or("an unnamed D-Bus error"),
                    if text.is_empty() {
                        String::new()
                    } else {
                        format!(": {text}")
                    }
                )));
            }
            if reply.kind != MessageType::MethodReturn {
                continue;
            }
            return Ok(Reply {
                endian: reply.endian,
                signature: reply.fields.signature.unwrap_or("").to_string(),
                body: reply.body_bytes().to_vec(),
            });
        }
        Err(io::Error::other(format!(
            "the session bus sent no reply to {member} after {MAX_UNRELATED_MESSAGES} unrelated messages"
        )))
    }

    fn finish_setup(&mut self) -> io::Result<()> {
        self.stream.set_read_timeout(None)?;
        self.stream.set_write_timeout(None)?;
        self.until = None;
        Ok(())
    }

    fn next_message(&mut self) -> io::Result<IncomingFrame> {
        read_service_frame(&mut self.stream)
    }

    fn write_frame(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.stream.write_all(bytes)
    }
}

fn connect_within(socket: &Path, timeout: Duration) -> io::Result<UnixStream> {
    let socket = socket.to_path_buf();
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("td-portal-connect".into())
        .spawn(move || {
            let _ = sender.send(UnixStream::connect(socket));
        })
        .map_err(|error| io::Error::other(format!("start the bounded connect worker: {error}")))?;
    receiver
        .recv_timeout(timeout)
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => {
                io::Error::new(io::ErrorKind::TimedOut, "the session-bus connect timed out")
            }
            mpsc::RecvTimeoutError::Disconnected => {
                io::Error::other("the session-bus connect worker exited without a result")
            }
        })?
}

fn auth_line(uid: u32) -> String {
    let identity: String = uid
        .to_string()
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("\0AUTH EXTERNAL {identity}\r\n")
}

fn read_frame(reader: &mut impl Read) -> io::Result<IncomingFrame> {
    let mut head = [0u8; message::HEADER_LEN];
    reader.read_exact(&mut head)?;
    let total = message::frame_len(&head)
        .map_err(message_error)?
        .ok_or_else(|| io::Error::other("a complete D-Bus header has no frame length"))?;
    if total > MAX_PORTAL_FRAME {
        let endian = Endian::from_byte(head[0])
            .ok_or_else(|| io::Error::other("an oversized D-Bus frame has bad endianness"))?;
        let mut fixed = wire::Reader::at(&head, 4, endian);
        let body_len = usize::try_from(fixed.u32().map_err(wire_error)?)
            .map_err(|_| io::Error::other("an oversized D-Bus body does not fit in usize"))?;
        let body_start = total
            .checked_sub(body_len)
            .ok_or_else(|| io::Error::other("an oversized D-Bus frame is shorter than its body"))?;
        if body_start < head.len() {
            return Err(io::Error::other(
                "an oversized D-Bus frame is shorter than its fixed header",
            ));
        }
        let mut prefix = Vec::with_capacity(body_start);
        prefix.extend_from_slice(&head);
        prefix.resize(body_start, 0);
        let tail = prefix
            .get_mut(head.len()..)
            .ok_or_else(|| io::Error::other("an oversized D-Bus header tail went missing"))?;
        reader.read_exact(tail)?;
        let call = oversized_call(&prefix, endian)?;
        let body_len = u64::try_from(body_len)
            .map_err(|_| io::Error::other("an oversized D-Bus body does not fit in u64"))?;
        let mut bounded = reader.take(body_len);
        let drained = io::copy(&mut bounded, &mut io::sink())?;
        if drained != body_len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "an oversized D-Bus frame ended before its declared length",
            ));
        }
        return Ok(IncomingFrame::Oversized { total, call });
    }
    let mut frame = Vec::with_capacity(total);
    frame.extend_from_slice(&head);
    let remaining = total
        .checked_sub(head.len())
        .ok_or_else(|| io::Error::other("a D-Bus frame is shorter than its fixed header"))?;
    frame.resize(total, 0);
    let tail = frame
        .get_mut(head.len()..)
        .ok_or_else(|| io::Error::other("the D-Bus frame tail went missing"))?;
    if tail.len() != remaining {
        return Err(io::Error::other(
            "the D-Bus frame length changed while reading",
        ));
    }
    reader.read_exact(tail)?;
    Ok(IncomingFrame::Message(frame))
}

fn read_service_frame(reader: &mut impl Read) -> io::Result<IncomingFrame> {
    loop {
        match read_frame(reader)? {
            frame @ IncomingFrame::Message(_) => return Ok(frame),
            frame @ IncomingFrame::Oversized { call: Some(_), .. } => return Ok(frame),
            IncomingFrame::Oversized { call: None, .. } => {}
        }
    }
}

fn oversized_call(prefix: &[u8], endian: Endian) -> io::Result<Option<OversizedCall>> {
    if prefix.get(1).copied() != Some(MessageType::MethodCall.code())
        || prefix
            .get(2)
            .is_some_and(|flags| flags & message::FLAG_NO_REPLY_EXPECTED != 0)
    {
        return Ok(None);
    }
    let mut serial_reader = wire::Reader::at(prefix, 8, endian);
    let serial = serial_reader.u32().map_err(wire_error)?;
    if serial == 0 {
        return Err(io::Error::other("an oversized D-Bus call has serial zero"));
    }
    let mut fields_reader = wire::Reader::at(prefix, 12, endian);
    let fields = fields_reader
        .value("a(yv)", Limits::NO_FDS)
        .map_err(wire_error)?
        .as_seq()
        .ok_or_else(|| io::Error::other("an oversized D-Bus call has malformed fields"))?;
    let mut sender = None;
    for entry in fields
        .values(wire::MAX_CONTAINER_ELEMENTS)
        .map_err(wire_error)?
    {
        let pair = entry
            .as_seq()
            .ok_or_else(|| io::Error::other("an oversized D-Bus field is not a structure"))?
            .values(2)
            .map_err(wire_error)?;
        let Some(Value::Byte(code)) = pair.first() else {
            return Err(io::Error::other(
                "an oversized D-Bus field has no byte code",
            ));
        };
        if *code != message::FieldCode::Sender.code() {
            continue;
        }
        if sender.is_some() {
            return Err(io::Error::other(
                "an oversized D-Bus call repeats its sender",
            ));
        }
        let variant = pair
            .get(1)
            .and_then(Value::as_seq)
            .ok_or_else(|| io::Error::other("an oversized D-Bus sender is not a variant"))?;
        if variant.signature() != message::FieldCode::Sender.signature() {
            return Err(io::Error::other(
                "an oversized D-Bus sender has the wrong type",
            ));
        }
        let values = variant.values(1).map_err(wire_error)?;
        let text = values
            .first()
            .and_then(Value::as_str)
            .ok_or_else(|| io::Error::other("an oversized D-Bus sender is not a string"))?;
        if !text.starts_with(':') || !name::valid_bus_name(text) {
            return Err(io::Error::other(
                "an oversized D-Bus call has an invalid sender",
            ));
        }
        sender = Some((*text).to_string());
    }
    let sender = sender
        .ok_or_else(|| io::Error::other("the broker forwarded an oversized call with no sender"))?;
    Ok(Some(OversizedCall {
        endian,
        serial,
        sender,
    }))
}

fn message_error(error: message::MessageError) -> io::Error {
    io::Error::other(format!("malformed D-Bus message: {error}"))
}

fn wire_error(error: WireError) -> io::Error {
    io::Error::other(format!("malformed D-Bus value: {error}"))
}

fn prepare(connection: &mut Connection) -> io::Result<String> {
    connection
        .call(
            BUS_NAME,
            PORTAL_ACTIVATION_PATH,
            PORTAL_ACTIVATION_INTERFACE,
            "Prepare",
            "as",
            |writer| writer.array("s", |writer| writer.string(PORTAL_NAME)),
        )?
        .one_string()
}

fn activate(connection: &mut Connection, token: &str) -> io::Result<()> {
    connection
        .call(
            BUS_NAME,
            PORTAL_ACTIVATION_PATH,
            PORTAL_ACTIVATION_INTERFACE,
            "Activate",
            "s",
            |writer| writer.string(token),
        )?
        .no_body()
}

fn request_name(connection: &mut Connection) -> io::Result<()> {
    let reply = connection.call(
        BUS_NAME,
        BUS_PATH,
        BUS_NAME,
        "RequestName",
        "su",
        |writer| {
            writer.string(PORTAL_NAME)?;
            writer.uint32(REQUEST_NAME_DO_NOT_QUEUE);
            Ok(())
        },
    )?;
    match reply.one_u32()? {
        REQUEST_NAME_PRIMARY_OWNER => Ok(()),
        other => Err(io::Error::other(format!(
            "RequestName returned {other}, not PRIMARY_OWNER"
        ))),
    }
}

fn child_argv(paths: &Paths, token: &str) -> Vec<OsString> {
    vec![
        OsString::from("exec-as"),
        OsString::from(PORTAL_USER),
        OsString::from("--"),
        OsString::from(PORTAL_PROGRAM),
        OsString::from("run"),
        OsString::from("--bus"),
        paths.bus.as_os_str().to_os_string(),
        OsString::from("--settings"),
        paths.settings.as_os_str().to_os_string(),
        OsString::from("--activation-token"),
        OsString::from(token),
    ]
}

fn supervise(paths: &Paths) -> Result<(), String> {
    if current_uid()? != 0 {
        return Err("td-portal supervise must run as root".into());
    }
    Settings::load(&paths.settings)?;
    let mut connection = Connection::open(&paths.bus, 0)
        .map_err(|error| format!("cannot prepare portal activation: {error}"))?;
    let token = prepare(&mut connection)
        .map_err(|error| format!("cannot prepare portal activation: {error}"))?;
    if !valid_token(&token) {
        return Err("the broker returned a malformed portal activation token".into());
    }
    connection
        .finish_setup()
        .map_err(|error| format!("cannot retain the portal supervisor connection: {error}"))?;
    let mut child = Command::new(PORTAL_LOGIN)
        .args(child_argv(paths, &token))
        .spawn()
        .map_err(|error| format!("cannot start the unprivileged portal child: {error}"))?;
    let status = child
        .wait()
        .map_err(|error| format!("cannot wait for the portal child: {error}"))?;
    Err(format!(
        "the portal child exited unexpectedly with {status}"
    ))
}

fn run(paths: &Paths) -> Result<(), String> {
    if current_uid()? != UI_UID {
        return Err(format!("td-portal run must run as uid {UI_UID}"));
    }
    let settings = Settings::load(&paths.settings)?;
    let token = paths
        .token
        .as_deref()
        .ok_or_else(|| "the portal child has no activation token".to_string())?;
    let mut connection = Connection::open(&paths.bus, UI_UID)
        .map_err(|error| format!("cannot connect the portal child to the session bus: {error}"))?;
    activate(&mut connection, token)
        .map_err(|error| format!("cannot activate the portal child: {error}"))?;
    request_name(&mut connection).map_err(|error| format!("cannot own {PORTAL_NAME}: {error}"))?;
    connection
        .finish_setup()
        .map_err(|error| format!("cannot enter portal service mode: {error}"))?;
    serve(&mut connection, &settings)
        .map_err(|error| format!("the portal service connection ended: {error}"))
}

fn serve(connection: &mut Connection, settings: &Settings) -> io::Result<()> {
    loop {
        let frame = match connection.next_message()? {
            IncomingFrame::Message(frame) => frame,
            IncomingFrame::Oversized {
                call: Some(call), ..
            } => {
                let serial = connection.next_serial()?;
                let reply = method_error(
                    call.endian,
                    serial,
                    call.serial,
                    &call.sender,
                    "org.freedesktop.DBus.Error.LimitsExceeded",
                    "the call is over the portal's decoded-frame ceiling",
                )?;
                connection.write_frame(&reply)?;
                continue;
            }
            IncomingFrame::Oversized { call: None, .. } => continue,
        };
        let (incoming, consumed) = message::decode(&frame, 0).map_err(message_error)?;
        if consumed != frame.len() {
            return Err(io::Error::other("a portal call carried trailing bytes"));
        }
        let serial = connection.next_serial()?;
        if let Some(reply) = dispatch(&incoming, settings, serial)? {
            connection.write_frame(&reply)?;
        }
    }
}

fn dispatch(call: &Message<'_>, settings: &Settings, serial: u32) -> io::Result<Option<Vec<u8>>> {
    if call.kind != MessageType::MethodCall {
        return Ok(None);
    }
    let wants_reply = call.flags & message::FLAG_NO_REPLY_EXPECTED == 0;
    let sender = call
        .fields
        .sender
        .ok_or_else(|| io::Error::other("the broker forwarded a call with no sender"))?;
    let answer = match (call.fields.path, call.fields.interface, call.fields.member) {
        (Some(PORTAL_PATH), Some(SETTINGS_INTERFACE), Some("ReadAll")) => {
            read_all_reply(call, settings, serial, sender)
        }
        (Some(PORTAL_PATH), Some(SETTINGS_INTERFACE), Some("Read")) => {
            read_reply(call, settings, serial, sender)
        }
        (Some(PORTAL_PATH), Some(PROPERTIES_INTERFACE), Some("Get")) => {
            property_get_reply(call, serial, sender)
        }
        (Some(PORTAL_PATH), Some(PROPERTIES_INTERFACE), Some("GetAll")) => {
            property_get_all_reply(call, serial, sender)
        }
        (Some(PORTAL_PATH), Some(PROPERTIES_INTERFACE), Some("Set")) => {
            property_set_reply(call, serial, sender)
        }
        (Some(PORTAL_PATH), Some(INTROSPECT_INTERFACE), Some("Introspect")) => {
            if exact_signature(call, "") {
                method_return(call.endian, serial, call.serial, sender, "s", |writer| {
                    writer.string(INTROSPECTION_XML)
                })
            } else {
                invalid_args(call, serial, sender, "Introspect takes no arguments")
            }
        }
        (_, Some(PEER_INTERFACE), Some("Ping")) => {
            if exact_signature(call, "") {
                method_return(call.endian, serial, call.serial, sender, "", |_| Ok(()))
            } else {
                invalid_args(call, serial, sender, "Ping takes no arguments")
            }
        }
        _ => method_error(
            call.endian,
            serial,
            call.serial,
            sender,
            "org.freedesktop.DBus.Error.UnknownMethod",
            "td-portal does not serve that method",
        ),
    }?;
    Ok(wants_reply.then_some(answer))
}

fn exact_signature(call: &Message<'_>, signature: &str) -> bool {
    call.fields.signature.unwrap_or("") == signature
}

fn two_strings<'a>(call: &'a Message<'a>) -> Option<(&'a str, &'a str)> {
    if !exact_signature(call, "ss") {
        return None;
    }
    match call.args() {
        [Value::Str(first), Value::Str(second)] => Some((first, second)),
        _ => None,
    }
}

fn read_all_reply(
    call: &Message<'_>,
    settings: &Settings,
    serial: u32,
    sender: &str,
) -> io::Result<Vec<u8>> {
    if !exact_signature(call, "as") {
        return invalid_args(call, serial, sender, "ReadAll takes one string array");
    }
    let Some(array) = call.args().first().and_then(Value::as_seq) else {
        return invalid_args(call, serial, sender, "ReadAll takes one string array");
    };
    let values = match array.values(MAX_NAMESPACE_FILTERS) {
        Ok(values) => values,
        Err(WireError::TooManyElements) => {
            return invalid_args(call, serial, sender, "too many ReadAll filters")
        }
        Err(error) => return Err(wire_error(error)),
    };
    let mut filters = Vec::with_capacity(values.len());
    for value in &values {
        let Some(filter) = value.as_str() else {
            return invalid_args(call, serial, sender, "ReadAll filters must be strings");
        };
        if filter.len() > MAX_NAMESPACE_BYTES {
            return invalid_args(call, serial, sender, "a ReadAll filter is too long");
        }
        filters.push(filter);
    }
    method_return(
        call.endian,
        serial,
        call.serial,
        sender,
        "a{sa{sv}}",
        |writer| settings.write_read_all(writer, &filters),
    )
}

fn read_reply(
    call: &Message<'_>,
    settings: &Settings,
    serial: u32,
    sender: &str,
) -> io::Result<Vec<u8>> {
    let Some((namespace, key)) = two_strings(call) else {
        return invalid_args(call, serial, sender, "Read takes namespace and key strings");
    };
    let Some(setting) = settings.setting(namespace, key) else {
        return method_error(
            call.endian,
            serial,
            call.serial,
            sender,
            "org.freedesktop.portal.Error.NotFound",
            "that setting is not published",
        );
    };
    method_return(call.endian, serial, call.serial, sender, "v", |writer| {
        setting.write_historical(writer)
    })
}

fn known_property_interface(interface: &str) -> bool {
    matches!(
        interface,
        SETTINGS_INTERFACE | PROPERTIES_INTERFACE | INTROSPECT_INTERFACE | PEER_INTERFACE
    )
}

fn unknown_interface(call: &Message<'_>, serial: u32, sender: &str) -> io::Result<Vec<u8>> {
    method_error(
        call.endian,
        serial,
        call.serial,
        sender,
        "org.freedesktop.DBus.Error.UnknownInterface",
        "that portal interface is not published",
    )
}

fn unknown_property(call: &Message<'_>, serial: u32, sender: &str) -> io::Result<Vec<u8>> {
    method_error(
        call.endian,
        serial,
        call.serial,
        sender,
        "org.freedesktop.DBus.Error.UnknownProperty",
        "that portal property is not published",
    )
}

fn property_get_reply(call: &Message<'_>, serial: u32, sender: &str) -> io::Result<Vec<u8>> {
    let Some((interface, property)) = two_strings(call) else {
        return invalid_args(
            call,
            serial,
            sender,
            "Get takes interface and property strings",
        );
    };
    if !interface.is_empty() && !known_property_interface(interface) {
        return unknown_interface(call, serial, sender);
    }
    if property != "version" || (!interface.is_empty() && interface != SETTINGS_INTERFACE) {
        return unknown_property(call, serial, sender);
    }
    method_return(call.endian, serial, call.serial, sender, "v", |writer| {
        writer.variant("u", |writer| {
            writer.uint32(PORTAL_VERSION);
            Ok(())
        })
    })
}

fn property_get_all_reply(call: &Message<'_>, serial: u32, sender: &str) -> io::Result<Vec<u8>> {
    if !exact_signature(call, "s") {
        return invalid_args(call, serial, sender, "GetAll takes one interface string");
    }
    let Some(interface) = call.args().first().and_then(Value::as_str) else {
        return invalid_args(call, serial, sender, "GetAll takes one interface string");
    };
    if !known_property_interface(interface) {
        return unknown_interface(call, serial, sender);
    }
    method_return(
        call.endian,
        serial,
        call.serial,
        sender,
        "a{sv}",
        |writer| match interface {
            SETTINGS_INTERFACE => writer.array("{sv}", |writer| {
                writer.dict_entry(|writer| {
                    writer.string("version")?;
                    writer.variant("u", |writer| {
                        writer.uint32(PORTAL_VERSION);
                        Ok(())
                    })
                })
            }),
            _ => writer.array("{sv}", |_| Ok(())),
        },
    )
}

fn property_set_reply(call: &Message<'_>, serial: u32, sender: &str) -> io::Result<Vec<u8>> {
    if !exact_signature(call, "ssv") {
        return invalid_args(
            call,
            serial,
            sender,
            "Set takes interface, property, and variant arguments",
        );
    }
    let [Value::Str(interface), Value::Str(property), Value::Variant(_)] = call.args() else {
        return invalid_args(
            call,
            serial,
            sender,
            "Set takes interface, property, and variant arguments",
        );
    };
    if !interface.is_empty() && !known_property_interface(interface) {
        return unknown_interface(call, serial, sender);
    }
    if *property != "version" || (!interface.is_empty() && *interface != SETTINGS_INTERFACE) {
        return unknown_property(call, serial, sender);
    }
    method_error(
        call.endian,
        serial,
        call.serial,
        sender,
        "org.freedesktop.DBus.Error.PropertyReadOnly",
        "the portal version is read-only",
    )
}

fn invalid_args(call: &Message<'_>, serial: u32, sender: &str, text: &str) -> io::Result<Vec<u8>> {
    method_error(
        call.endian,
        serial,
        call.serial,
        sender,
        "org.freedesktop.DBus.Error.InvalidArgs",
        text,
    )
}

fn method_return<F>(
    endian: Endian,
    serial: u32,
    reply_serial: u32,
    destination: &str,
    signature: &str,
    fill: F,
) -> io::Result<Vec<u8>>
where
    F: FnOnce(&mut Writer) -> Result<(), WireError>,
{
    message::Builder::method_return(endian, reply_serial)
        .destination(destination)
        .serial(serial)
        .body(signature, fill)
        .map_err(wire_error)?
        .encode()
        .map_err(message_error)
}

fn method_error(
    endian: Endian,
    serial: u32,
    reply_serial: u32,
    destination: &str,
    name: &str,
    text: &str,
) -> io::Result<Vec<u8>> {
    message::Builder::error(endian, name, reply_serial)
        .destination(destination)
        .serial(serial)
        .body("s", |writer| writer.string(text))
        .map_err(wire_error)?
        .encode()
        .map_err(message_error)
}

fn expected_read_all(settings: &Settings) -> io::Result<Vec<u8>> {
    let mut writer = Writer::new(Endian::Little);
    settings
        .write_read_all(&mut writer, &[APPEARANCE, GNOME_INTERFACE])
        .map_err(wire_error)?;
    Ok(writer.into_bytes())
}

fn expected_version() -> io::Result<Vec<u8>> {
    let mut writer = Writer::new(Endian::Little);
    writer
        .variant("u", |writer| {
            writer.uint32(PORTAL_VERSION);
            Ok(())
        })
        .map_err(wire_error)?;
    Ok(writer.into_bytes())
}

fn probe(paths: &Paths) -> Result<(), String> {
    if current_uid()? != UI_UID {
        return Err(format!("td-portal probe must run as uid {UI_UID}"));
    }
    let settings = Settings::load(&paths.settings)?;
    let mut connection = Connection::open(&paths.bus, UI_UID)
        .map_err(|error| format!("cannot connect the portal probe: {error}"))?;
    let version = connection
        .call(
            PORTAL_NAME,
            PORTAL_PATH,
            PROPERTIES_INTERFACE,
            "Get",
            "ss",
            |writer| {
                writer.string(SETTINGS_INTERFACE)?;
                writer.string("version")
            },
        )
        .map_err(|error| format!("the Settings version call failed: {error}"))?;
    if version.signature != "v" || version.body != expected_version().map_err(|e| e.to_string())? {
        return Err("the live Settings version reply is not exact version 1".into());
    }
    let all = connection
        .call(
            PORTAL_NAME,
            PORTAL_PATH,
            SETTINGS_INTERFACE,
            "ReadAll",
            "as",
            |writer| {
                writer.array("s", |writer| {
                    writer.string(APPEARANCE)?;
                    writer.string(GNOME_INTERFACE)
                })
            },
        )
        .map_err(|error| format!("the Settings.ReadAll call failed: {error}"))?;
    if all.signature != "a{sa{sv}}"
        || all.body != expected_read_all(&settings).map_err(|e| e.to_string())?
    {
        return Err("the live Settings.ReadAll reply differs from the session file".into());
    }
    println!("{READY_MARKER}");
    Ok(())
}

fn selftest() -> Result<(), String> {
    let settings = Settings::parse(settings::DEFAULT_CONFIG)?;
    let body = expected_read_all(&settings).map_err(|error| error.to_string())?;
    let values = wire::read_body(&body, "a{sa{sv}}", Endian::Little, Limits::NO_FDS)
        .map_err(|error| format!("selftest cannot decode Settings.ReadAll: {error}"))?;
    let count = values
        .first()
        .and_then(Value::as_seq)
        .ok_or_else(|| "selftest did not produce a namespace dictionary".to_string())?
        .values(NAMESPACE_COUNT)
        .map_err(|error| format!("selftest cannot walk Settings.ReadAll: {error}"))?
        .len();
    if count != NAMESPACE_COUNT {
        return Err(format!("selftest produced {count} namespaces"));
    }
    println!("TD-PORTAL-SELFTEST-OK");
    Ok(())
}

fn run_main(args: &[String]) -> Result<(), String> {
    let command = args.first().ok_or_else(usage)?;
    match command.as_str() {
        "supervise" => supervise(&parse_paths(args.get(1..).ok_or_else(usage)?, false)?),
        "run" => run(&parse_paths(args.get(1..).ok_or_else(usage)?, true)?),
        "probe" => probe(&parse_paths(args.get(1..).ok_or_else(usage)?, false)?),
        "selftest" if args.get(1).is_none() => selftest(),
        _ => Err(usage()),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(error) = run_main(&args) {
        eprintln!("td-portal: {error}");
        process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn call<F>(interface: &str, member: &str, signature: &str, fill: F) -> Vec<u8>
    where
        F: FnOnce(&mut Writer) -> Result<(), WireError>,
    {
        message::Builder::method_call(Endian::Little, PORTAL_PATH, Some(interface), member)
            .destination(PORTAL_NAME)
            .sender(":1.9")
            .serial(7)
            .body(signature, fill)
            .unwrap()
            .encode()
            .unwrap()
    }

    #[test]
    fn the_three_commands_have_closed_flag_grammars() {
        let base = strings(&[
            "--bus",
            "/run/user/1000/bus",
            "--settings",
            "/etc/td-settings",
        ]);
        assert!(parse_paths(&base, false).is_ok());
        assert!(parse_paths(&base, true).is_err());
        let mut run = base.clone();
        run.extend(strings(&[
            "--activation-token",
            "00112233445566778899aabbccddeeff",
        ]));
        assert!(parse_paths(&run, true).is_ok());
        for bad in [
            strings(&["--bus", "relative", "--settings", "/etc/x"]),
            strings(&[
                "--bus",
                "/run/bus",
                "--bus",
                "/run/other",
                "--settings",
                "/etc/x",
            ]),
            strings(&["--bus", "/run/bus", "--settings", "/etc/x", "--extra", "x"]),
        ] {
            assert!(parse_paths(&bad, false).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn the_supervisor_execs_one_literal_direct_child() {
        let paths = Paths {
            bus: PathBuf::from("/run/user/1000/bus"),
            settings: PathBuf::from("/etc/td-portal-settings"),
            token: None,
        };
        let args = child_argv(&paths, "00112233445566778899aabbccddeeff");
        assert_eq!(PORTAL_LOGIN, "/bin/td-login");
        assert_eq!(
            args,
            strings(&[
                "exec-as",
                "tester",
                "--",
                "/bin/td-portal",
                "run",
                "--bus",
                "/run/user/1000/bus",
                "--settings",
                "/etc/td-portal-settings",
                "--activation-token",
                "00112233445566778899aabbccddeeff",
            ])
            .into_iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn read_all_returns_the_exact_session_settings() {
        let settings = Settings::parse(settings::DEFAULT_CONFIG).unwrap();
        let bytes = call(SETTINGS_INTERFACE, "ReadAll", "as", |writer| {
            writer.array("s", |writer| {
                writer.string(APPEARANCE)?;
                writer.string(GNOME_INTERFACE)
            })
        });
        let (request, _) = message::decode(&bytes, 0).unwrap();
        let reply = dispatch(&request, &settings, 10).unwrap().unwrap();
        let (reply, _) = message::decode(&reply, 0).unwrap();
        assert_eq!(reply.kind, MessageType::MethodReturn);
        assert_eq!(reply.fields.reply_serial, Some(7));
        assert_eq!(reply.fields.destination, Some(":1.9"));
        assert_eq!(reply.fields.signature, Some("a{sa{sv}}"));
        assert_eq!(reply.body_bytes(), expected_read_all(&settings).unwrap());
    }

    #[test]
    fn properties_get_returns_version_one_inside_a_variant() {
        let settings = Settings::parse(settings::DEFAULT_CONFIG).unwrap();
        let bytes = call(PROPERTIES_INTERFACE, "Get", "ss", |writer| {
            writer.string(SETTINGS_INTERFACE)?;
            writer.string("version")
        });
        let (request, _) = message::decode(&bytes, 0).unwrap();
        let reply = dispatch(&request, &settings, 11).unwrap().unwrap();
        let (reply, _) = message::decode(&reply, 0).unwrap();
        assert_eq!(reply.fields.signature, Some("v"));
        assert_eq!(reply.body_bytes(), expected_version().unwrap());
    }

    #[test]
    fn properties_obey_empty_interface_and_read_only_rules() {
        let settings = Settings::parse(settings::DEFAULT_CONFIG).unwrap();
        let get = call(PROPERTIES_INTERFACE, "Get", "ss", |writer| {
            writer.string("")?;
            writer.string("version")
        });
        let (request, _) = message::decode(&get, 0).unwrap();
        let reply = dispatch(&request, &settings, 12).unwrap().unwrap();
        let (reply, _) = message::decode(&reply, 0).unwrap();
        assert_eq!(reply.body_bytes(), expected_version().unwrap());

        let get_all = call(PROPERTIES_INTERFACE, "GetAll", "s", |writer| {
            writer.string(PEER_INTERFACE)
        });
        let (request, _) = message::decode(&get_all, 0).unwrap();
        let reply = dispatch(&request, &settings, 13).unwrap().unwrap();
        let (reply, _) = message::decode(&reply, 0).unwrap();
        assert_eq!(reply.fields.signature, Some("a{sv}"));
        let values = reply
            .args()
            .first()
            .and_then(Value::as_seq)
            .unwrap()
            .values(1)
            .unwrap();
        assert!(values.is_empty());

        let set = call(PROPERTIES_INTERFACE, "Set", "ssv", |writer| {
            writer.string(SETTINGS_INTERFACE)?;
            writer.string("version")?;
            writer.variant("u", |writer| {
                writer.uint32(2);
                Ok(())
            })
        });
        let (request, _) = message::decode(&set, 0).unwrap();
        let reply = dispatch(&request, &settings, 14).unwrap().unwrap();
        let (reply, _) = message::decode(&reply, 0).unwrap();
        assert_eq!(
            reply.fields.error_name,
            Some("org.freedesktop.DBus.Error.PropertyReadOnly")
        );
    }

    #[test]
    fn a_reply_expected_oversized_call_gets_an_error_before_the_next_call() {
        let payload = "x".repeat(MAX_PORTAL_FRAME);
        let oversized = call(SETTINGS_INTERFACE, "ReadAll", "s", |writer| {
            writer.string(&payload)
        });
        assert!(oversized.len() > MAX_PORTAL_FRAME);
        let ping = call(PEER_INTERFACE, "Ping", "", |_| Ok(()));
        let mut stream = oversized;
        stream.extend_from_slice(&ping);
        let mut cursor = io::Cursor::new(stream);
        let IncomingFrame::Oversized {
            total,
            call: Some(rejected),
        } = read_service_frame(&mut cursor).unwrap()
        else {
            panic!("the oversized call was not retained for an error reply");
        };
        assert!(total > MAX_PORTAL_FRAME);
        assert_eq!(rejected.serial, 7);
        assert_eq!(rejected.sender, ":1.9");
        let error = method_error(
            rejected.endian,
            20,
            rejected.serial,
            &rejected.sender,
            "org.freedesktop.DBus.Error.LimitsExceeded",
            "the call is over the portal's decoded-frame ceiling",
        )
        .unwrap();
        let (error, _) = message::decode(&error, 0).unwrap();
        assert_eq!(
            error.fields.error_name,
            Some("org.freedesktop.DBus.Error.LimitsExceeded")
        );
        assert_eq!(error.fields.reply_serial, Some(7));
        assert_eq!(error.fields.destination, Some(":1.9"));
        let IncomingFrame::Message(next) = read_service_frame(&mut cursor).unwrap() else {
            panic!("the legal call after an oversized call went missing");
        };
        assert_eq!(next, ping);
        assert_eq!(cursor.position(), cursor.get_ref().len() as u64);
    }

    #[test]
    fn a_no_reply_oversized_call_is_drained_without_an_answer() {
        let payload = "x".repeat(MAX_PORTAL_FRAME);
        let oversized = message::Builder::method_call(
            Endian::Little,
            PORTAL_PATH,
            Some(SETTINGS_INTERFACE),
            "ReadAll",
        )
        .destination(PORTAL_NAME)
        .sender(":1.9")
        .flags(message::FLAG_NO_REPLY_EXPECTED)
        .serial(7)
        .body("s", |writer| writer.string(&payload))
        .unwrap()
        .encode()
        .unwrap();
        assert!(oversized.len() > MAX_PORTAL_FRAME);
        let ping = call(PEER_INTERFACE, "Ping", "", |_| Ok(()));
        let mut stream = oversized;
        stream.extend_from_slice(&ping);
        let mut cursor = io::Cursor::new(stream);
        let IncomingFrame::Message(next) = read_service_frame(&mut cursor).unwrap() else {
            panic!("a no-reply oversized call was retained for an answer");
        };
        assert_eq!(next, ping);
        assert_eq!(cursor.position(), cursor.get_ref().len() as u64);
    }

    #[test]
    fn the_service_answers_an_oversized_call_and_keeps_serving() {
        let payload = "x".repeat(MAX_PORTAL_FRAME);
        let oversized = call(SETTINGS_INTERFACE, "ReadAll", "s", |writer| {
            writer.string(&payload)
        });
        let ping = call(PEER_INTERFACE, "Ping", "", |_| Ok(()));
        let (service_stream, mut client_stream) = UnixStream::pair().unwrap();
        client_stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let settings = Settings::parse(settings::DEFAULT_CONFIG).unwrap();
        thread::scope(|scope| {
            let service = scope.spawn(move || {
                let mut connection = Connection {
                    stream: service_stream,
                    serial: 19,
                    unique: Some(":1.10".into()),
                    until: None,
                };
                serve(&mut connection, &settings)
            });
            client_stream.write_all(&oversized).unwrap();
            client_stream.write_all(&ping).unwrap();

            let IncomingFrame::Message(error) = read_frame(&mut client_stream).unwrap() else {
                panic!("the service did not answer the oversized call");
            };
            let (error, _) = message::decode(&error, 0).unwrap();
            assert_eq!(
                error.fields.error_name,
                Some("org.freedesktop.DBus.Error.LimitsExceeded")
            );
            assert_eq!(error.fields.reply_serial, Some(7));
            assert_eq!(error.fields.destination, Some(":1.9"));

            let IncomingFrame::Message(reply) = read_frame(&mut client_stream).unwrap() else {
                panic!("the service stopped before answering the next call");
            };
            let (reply, _) = message::decode(&reply, 0).unwrap();
            assert_eq!(reply.kind, MessageType::MethodReturn);
            assert_eq!(reply.fields.reply_serial, Some(7));
            drop(client_stream);
            assert!(service.join().unwrap().is_err());
        });
    }

    #[test]
    fn ready_marker_is_derived_from_the_published_contract() {
        assert_eq!(
            READY_MARKER,
            format!(
                "TD-PORTAL-READY namespaces={NAMESPACE_COUNT} settings={} version={PORTAL_VERSION}",
                settings::SETTING_COUNT
            )
        );
    }

    #[test]
    fn historic_read_wraps_the_concrete_setting_in_a_second_variant() {
        let settings = Settings::parse(settings::DEFAULT_CONFIG).unwrap();
        let bytes = call(SETTINGS_INTERFACE, "Read", "ss", |writer| {
            writer.string(APPEARANCE)?;
            writer.string("color-scheme")
        });
        let (request, _) = message::decode(&bytes, 0).unwrap();
        let reply = dispatch(&request, &settings, 12).unwrap().unwrap();
        let (reply, _) = message::decode(&reply, 0).unwrap();
        assert_eq!(reply.fields.signature, Some("v"));
        let outer = reply.args();
        let inner = outer.first().and_then(Value::as_seq).unwrap();
        assert_eq!(inner.signature(), "v");
        let concrete = inner.values(1).unwrap();
        let value = concrete.first().and_then(Value::as_seq).unwrap();
        assert_eq!(value.signature(), "u");
        assert_eq!(value.values(1).unwrap(), vec![Value::Uint32(1)]);
    }

    #[test]
    fn bad_arguments_and_unknown_settings_are_answered() {
        let settings = Settings::parse(settings::DEFAULT_CONFIG).unwrap();
        let malformed = call(SETTINGS_INTERFACE, "ReadAll", "s", |writer| {
            writer.string(APPEARANCE)
        });
        let (request, _) = message::decode(&malformed, 0).unwrap();
        let reply = dispatch(&request, &settings, 12).unwrap().unwrap();
        let (reply, _) = message::decode(&reply, 0).unwrap();
        assert_eq!(
            reply.fields.error_name,
            Some("org.freedesktop.DBus.Error.InvalidArgs")
        );

        let absent = call(SETTINGS_INTERFACE, "Read", "ss", |writer| {
            writer.string(APPEARANCE)?;
            writer.string("not-there")
        });
        let (request, _) = message::decode(&absent, 0).unwrap();
        let reply = dispatch(&request, &settings, 13).unwrap().unwrap();
        let (reply, _) = message::decode(&reply, 0).unwrap();
        assert_eq!(
            reply.fields.error_name,
            Some("org.freedesktop.portal.Error.NotFound")
        );
    }

    #[test]
    fn read_all_filter_count_is_a_replied_error_not_a_service_exit() {
        let settings = Settings::parse(settings::DEFAULT_CONFIG).unwrap();
        let oversized = call(SETTINGS_INTERFACE, "ReadAll", "as", |writer| {
            writer.array("s", |writer| {
                for _ in 0..=MAX_NAMESPACE_FILTERS {
                    writer.string(APPEARANCE)?;
                }
                Ok(())
            })
        });
        let (request, _) = message::decode(&oversized, 0).unwrap();
        let reply = dispatch(&request, &settings, 14).unwrap().unwrap();
        let (reply, _) = message::decode(&reply, 0).unwrap();
        assert_eq!(
            reply.fields.error_name,
            Some("org.freedesktop.DBus.Error.InvalidArgs")
        );
    }

    #[test]
    fn no_reply_expected_still_dispatches_without_emitting() {
        let settings = Settings::parse(settings::DEFAULT_CONFIG).unwrap();
        let bytes = message::Builder::method_call(
            Endian::Little,
            PORTAL_PATH,
            Some(PEER_INTERFACE),
            "Ping",
        )
        .destination(PORTAL_NAME)
        .sender(":1.9")
        .flags(message::FLAG_NO_REPLY_EXPECTED)
        .serial(7)
        .encode()
        .unwrap();
        let (request, _) = message::decode(&bytes, 0).unwrap();
        assert!(dispatch(&request, &settings, 10).unwrap().is_none());
    }
}
