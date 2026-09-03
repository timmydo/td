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
//! The service owns the synchronous Settings interface and the bounded shared
//! portal handle core. A root supervisor obtains td-busd's one-shot ownership
//! capability, starts its direct unprivileged child through `td-login
//! exec-as`, and stays alive while that child owns
//! `org.freedesktop.portal.Desktop`.

mod file_chooser;
#[path = "../../td-compositor/src/font.rs"]
mod font;
#[path = "../../td-compositor/src/font_data.rs"]
mod font_data;
mod handles;
#[path = "../../td-compositor/src/keyboard.rs"]
#[allow(
    dead_code,
    reason = "the shared keyboard profile is broader than one chooser"
)]
mod keyboard;
#[path = "../../td-compositor/src/filter.rs"]
mod list_filter;
#[path = "../../td-busd/src/message.rs"]
#[allow(
    dead_code,
    reason = "the shared broker codec is broader than one portal"
)]
mod message;
#[path = "../../td-busd/src/name.rs"]
mod name;
mod settings;
mod sys;
mod wayland_channel;
mod wayland_dialog;
#[path = "../../td-compositor/src/wire.rs"]
#[allow(
    dead_code,
    reason = "the shared compositor codec is broader than one registry probe"
)]
mod wayland_wire;
#[path = "../../td-busd/src/wire.rs"]
#[allow(
    dead_code,
    reason = "the shared broker codec is broader than one portal"
)]
mod wire;

mod scene {
    #[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
    pub struct SurfaceKey {
        pub client: u64,
        pub object: u32,
    }
}

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use handles::{HandleKind, Handles, LookupError, ReserveError};
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
const BACKGROUND_INTERFACE: &str = "org.freedesktop.portal.Background";
const FILE_CHOOSER_INTERFACE: &str = "org.freedesktop.portal.FileChooser";
const UNAVAILABLE_INTERFACES: [(&str, &str); 5] = [
    ("org.freedesktop.portal.ScreenCast", "CreateSession"),
    ("org.freedesktop.portal.RemoteDesktop", "CreateSession"),
    ("org.freedesktop.portal.Camera", "AccessCamera"),
    ("org.freedesktop.portal.Secret", "RetrieveSecret"),
    ("org.freedesktop.portal.Print", "Print"),
];
const PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";
const INTROSPECT_INTERFACE: &str = "org.freedesktop.DBus.Introspectable";
const PEER_INTERFACE: &str = "org.freedesktop.DBus.Peer";
const UNKNOWN_INTERFACE_ERROR: &str = "org.freedesktop.DBus.Error.UnknownInterface";
const UNKNOWN_INTERFACE_TEXT: &str = "that portal interface is not published";
const UNKNOWN_METHOD_ERROR: &str = "org.freedesktop.DBus.Error.UnknownMethod";
const UNKNOWN_METHOD_TEXT: &str = "td-portal does not serve that method";
const SETTINGS_VERSION: u32 = 1;
const BACKGROUND_VERSION: u32 = 1;
const FILE_CHOOSER_VERSION: u32 = 3;
const SESSION_VERSION: u32 = 1;
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
const MAX_PORTAL_OPTIONS: usize = 32;
const MAX_PENDING_IDENTITIES: usize = 16;
const MAX_PENDING_IDENTITIES_PER_OWNER: usize = 4;
const MAX_PENDING_OPENS: usize = 15;
const MAX_PENDING_OPENS_PER_OWNER: usize = 3;
const MAX_ACTIVE_FILE_CHOOSERS: usize = 1;
const MAX_QUEUED_SERVICE_EVENTS: usize = 32;
const MAX_FILE_CHOOSER_TITLE_BYTES: usize = 256;
const OWNER_AUDIT_INTERVAL: Duration = Duration::from_secs(10);
const FILE_CHOOSER_SOCKET: &str = "/run/user/1000/td-portal-wayland-0";
const FILE_CHOOSER_RUNTIME: &str = "/run/user/1000";
const FIREFOX_HOST_DOWNLOADS: &str = "/var/home/tester/Downloads";
const FIREFOX_GUEST_DOWNLOADS: &str = "/home/td/Downloads";
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(20);
const OWNER_MATCH: &str = "type='signal',sender='org.freedesktop.DBus',interface='org.freedesktop.DBus',member='NameOwnerChanged'";
pub const READY_MARKER: &str = "TD-PORTAL-READY namespaces=2 settings=10 version=1";
pub const REQUEST_READY_MARKER: &str = "TD-PORTAL-REQUEST-READY response=2";
pub const UNAVAILABLE_READY_MARKER: &str =
    "TD-PORTAL-UNAVAILABLE-READY interfaces=5 error=UnknownInterface";
const BACKGROUND_DENIAL_BODY: &[u8] = &[
    2, 0, 0, 0, 48, 0, 0, 0, 10, 0, 0, 0, 98, 97, 99, 107, 103, 114, 111, 117, 110, 100, 0, 1, 98,
    0, 0, 0, 0, 0, 0, 0, 9, 0, 0, 0, 97, 117, 116, 111, 115, 116, 97, 114, 116, 0, 1, 98, 0, 0, 0,
    0, 0, 0, 0, 0,
];

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
  <interface name="org.freedesktop.portal.Background">
    <method name="RequestBackground">
      <arg type="s" name="parent_window" direction="in"/>
      <arg type="a{sv}" name="options" direction="in"/>
      <arg type="o" name="handle" direction="out"/>
    </method>
    <property name="version" type="u" access="read"/>
  </interface>
  <interface name="org.freedesktop.portal.FileChooser">
    <method name="OpenFile">
      <arg type="s" name="parent_window" direction="in"/>
      <arg type="s" name="title" direction="in"/>
      <arg type="a{sv}" name="options" direction="in"/>
      <arg type="o" name="handle" direction="out"/>
    </method>
    <method name="SaveFile">
      <arg type="s" name="parent_window" direction="in"/>
      <arg type="s" name="title" direction="in"/>
      <arg type="a{sv}" name="options" direction="in"/>
      <arg type="o" name="handle" direction="out"/>
    </method>
    <method name="SaveFiles">
      <arg type="s" name="parent_window" direction="in"/>
      <arg type="s" name="title" direction="in"/>
      <arg type="a{sv}" name="options" direction="in"/>
      <arg type="o" name="handle" direction="out"/>
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

const REQUEST_INTROSPECTION_XML: &str = r#"<node>
  <interface name="org.freedesktop.portal.Request">
    <method name="Close"/>
    <signal name="Response"><arg type="u"/><arg type="a{sv}"/></signal>
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
  <interface name="org.freedesktop.DBus.Peer"><method name="Ping"/></interface>
</node>"#;

const SESSION_INTROSPECTION_XML: &str = r#"<node>
  <interface name="org.freedesktop.portal.Session">
    <method name="Close"/>
    <signal name="Closed"><arg type="a{sv}"/></signal>
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
  <interface name="org.freedesktop.DBus.Peer"><method name="Ping"/></interface>
</node>"#;

fn usage() -> String {
    "usage: td-portal supervise --bus PATH --settings PATH | \
     td-portal run --bus PATH --settings PATH --activation-token TOKEN | \
     td-portal probe --bus PATH --settings PATH | \
     td-portal channel-probe --wayland PATH | td-portal selftest"
        .into()
}

fn parse_channel_path(args: &[String]) -> Result<PathBuf, String> {
    match args {
        [flag, value] if flag == "--wayland" => {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                return Err("private portal Wayland socket path must be absolute".into());
            }
            Ok(path)
        }
        _ => Err(usage()),
    }
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
    sender: String,
    signature: String,
    body: Vec<u8>,
}

#[derive(Debug)]
struct RemoteError {
    endian: Endian,
    sender: String,
    name: Option<String>,
    signature: String,
    body: Vec<u8>,
}

#[derive(Debug)]
enum CallOutcome {
    Return(Reply),
    Error(RemoteError),
}

struct CallGuard<'a> {
    signature: &'a str,
    early_response_path: Option<&'a str>,
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

    fn one_object_path(&self) -> io::Result<String> {
        if self.signature != "o" {
            return Err(io::Error::other(format!(
                "expected an object-path reply, got {:?}",
                self.signature
            )));
        }
        let values = self.values()?;
        match values.as_slice() {
            [Value::ObjectPath(path)] => Ok((*path).to_string()),
            _ => Err(io::Error::other(
                "the object-path reply body has another shape",
            )),
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

impl RemoteError {
    fn values(&self) -> io::Result<Vec<Value<'_>>> {
        wire::read_body(&self.body, &self.signature, self.endian, Limits::NO_FDS)
            .map_err(wire_error)
    }

    fn is_exact_string(&self, name: &str, text: &str) -> io::Result<bool> {
        if self.name.as_deref() != Some(name) || self.signature != "s" {
            return Ok(false);
        }
        let values = self.values()?;
        Ok(matches!(values.as_slice(), [Value::Str(value)] if *value == text))
    }

    fn diagnostic(&self, member: &str) -> String {
        let name = self.name.as_deref().unwrap_or("an unnamed D-Bus error");
        let text = if self.signature == "s" {
            self.values()
                .ok()
                .and_then(|values| match values.as_slice() {
                    [Value::Str(text)] => Some(*text),
                    _ => None,
                })
        } else {
            None
        };
        match text {
            Some(text) if !text.is_empty() => format!(
                "{member} was refused as {name}: {}",
                escape_diagnostic(text)
            ),
            _ => format!(
                "{member} was refused as {name} with signature {:?} and {} body bytes",
                self.signature,
                self.body.len()
            ),
        }
    }
}

fn escape_diagnostic(text: &str) -> String {
    text.chars().flat_map(char::escape_default).collect()
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
        next_serial_value(&mut self.serial)
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
        self.call_guarded(
            destination,
            path,
            interface,
            member,
            CallGuard {
                signature,
                early_response_path: None,
            },
            fill,
        )
    }

    fn call_outcome<F>(
        &mut self,
        destination: &str,
        path: &str,
        interface: &str,
        member: &str,
        signature: &str,
        fill: F,
    ) -> io::Result<CallOutcome>
    where
        F: FnOnce(&mut Writer) -> Result<(), WireError>,
    {
        self.call_outcome_guarded(
            destination,
            path,
            interface,
            member,
            CallGuard {
                signature,
                early_response_path: None,
            },
            fill,
        )
    }

    fn call_guarded<F>(
        &mut self,
        destination: &str,
        path: &str,
        interface: &str,
        member: &str,
        guard: CallGuard<'_>,
        fill: F,
    ) -> io::Result<Reply>
    where
        F: FnOnce(&mut Writer) -> Result<(), WireError>,
    {
        match self.call_outcome_guarded(destination, path, interface, member, guard, fill)? {
            CallOutcome::Return(reply) => Ok(reply),
            CallOutcome::Error(error) => Err(io::Error::other(error.diagnostic(member))),
        }
    }

    fn call_outcome_guarded<F>(
        &mut self,
        destination: &str,
        path: &str,
        interface: &str,
        member: &str,
        guard: CallGuard<'_>,
        fill: F,
    ) -> io::Result<CallOutcome>
    where
        F: FnOnce(&mut Writer) -> Result<(), WireError>,
    {
        let serial = self.next_serial()?;
        let frame = message::Builder::method_call(Endian::Little, path, Some(interface), member)
            .destination(destination)
            .serial(serial)
            .body(guard.signature, fill)
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
            if guard.early_response_path.is_some_and(|path| {
                is_directed_request_response(&reply, path, self.unique.as_deref())
            }) {
                return Err(io::Error::other(
                    "Request.Response arrived before its method reply",
                ));
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
                return Ok(CallOutcome::Error(RemoteError {
                    endian: reply.endian,
                    sender: sender.to_string(),
                    name: reply.fields.error_name.map(str::to_string),
                    signature: reply.fields.signature.unwrap_or("").to_string(),
                    body: reply.body_bytes().to_vec(),
                }));
            }
            if reply.kind != MessageType::MethodReturn {
                continue;
            }
            return Ok(CallOutcome::Return(Reply {
                endian: reply.endian,
                sender: sender.to_string(),
                signature: reply.fields.signature.unwrap_or("").to_string(),
                body: reply.body_bytes().to_vec(),
            }));
        }
        Err(io::Error::other(format!(
            "the session bus sent no reply to {member} after {MAX_UNRELATED_MESSAGES} unrelated messages"
        )))
    }

    fn wait_request_response(&mut self, path: &str, portal_sender: &str) -> io::Result<Reply> {
        for _ in 0..=MAX_UNRELATED_MESSAGES {
            let bytes = match read_frame(&mut self.timed()?)? {
                IncomingFrame::Message(bytes) => bytes,
                IncomingFrame::Oversized { total, .. } => {
                    return Err(io::Error::other(format!(
                        "the Request.Response is {total} bytes, over the portal ceiling"
                    )))
                }
            };
            let (signal, consumed) = message::decode(&bytes, 0).map_err(message_error)?;
            if consumed != bytes.len() {
                return Err(io::Error::other("a D-Bus signal carried trailing bytes"));
            }
            if signal.kind != MessageType::Signal
                || signal.fields.path != Some(path)
                || signal.fields.interface != Some(handles::REQUEST_INTERFACE)
                || signal.fields.member != Some("Response")
                || signal.fields.sender != Some(portal_sender)
            {
                continue;
            }
            let unique = self
                .unique
                .as_deref()
                .ok_or_else(|| io::Error::other("the portal probe has no unique name"))?;
            if signal.fields.destination != Some(unique) {
                return Err(io::Error::other(
                    "Request.Response was not directed to its caller",
                ));
            }
            return Ok(Reply {
                endian: signal.endian,
                sender: portal_sender.to_string(),
                signature: signal.fields.signature.unwrap_or("").to_string(),
                body: signal.body_bytes().to_vec(),
            });
        }
        Err(io::Error::other(format!(
            "the session bus sent no Request.Response after {MAX_UNRELATED_MESSAGES} unrelated messages"
        )))
    }

    fn finish_setup(&mut self) -> io::Result<()> {
        self.stream.set_read_timeout(None)?;
        self.stream.set_write_timeout(None)?;
        self.until = None;
        Ok(())
    }

    fn write_frame(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.stream.write_all(bytes)
    }

    fn service_reader(&self) -> io::Result<UnixStream> {
        self.stream.try_clone()
    }
}

fn is_directed_request_response(
    message: &Message<'_>,
    path: &str,
    destination: Option<&str>,
) -> bool {
    message.kind == MessageType::Signal
        && message.fields.path == Some(path)
        && message.fields.interface == Some(handles::REQUEST_INTERFACE)
        && message.fields.member == Some("Response")
        && message.fields.destination == destination
}

fn next_serial_value(serial: &mut u32) -> io::Result<u32> {
    *serial = serial
        .checked_add(1)
        .ok_or_else(|| io::Error::other("this D-Bus connection ran out of serials"))?;
    Ok(*serial)
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

fn subscribe_to_owner_departures(connection: &mut Connection) -> io::Result<()> {
    connection
        .call(BUS_NAME, BUS_PATH, BUS_NAME, "AddMatch", "s", |writer| {
            writer.string(OWNER_MATCH)
        })?
        .no_body()
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
    subscribe_to_owner_departures(&mut connection)
        .map_err(|error| format!("cannot subscribe to caller departures: {error}"))?;
    connection
        .finish_setup()
        .map_err(|error| format!("cannot enter portal service mode: {error}"))?;
    serve(&mut connection, &settings)
        .map_err(|error| format!("the portal service connection ended: {error}"))
}

fn serve(connection: &mut Connection, settings: &Settings) -> io::Result<()> {
    let mut reader = connection.service_reader()?;
    let (sender, receiver) = mpsc::sync_channel(MAX_QUEUED_SERVICE_EVENTS);
    let bus_sender = sender.clone();
    thread::Builder::new()
        .name("td-portal-bus-reader".into())
        .spawn(move || loop {
            let event = read_service_frame(&mut reader);
            let terminal = event.is_err();
            if bus_sender.send(ServiceEvent::Bus(event)).is_err() || terminal {
                break;
            }
        })
        .map_err(|error| io::Error::other(format!("start portal bus reader: {error}")))?;
    let audit_sender = sender.clone();
    thread::Builder::new()
        .name("td-portal-owner-audit".into())
        .spawn(move || loop {
            thread::sleep(OWNER_AUDIT_INTERVAL);
            if audit_sender.send(ServiceEvent::Audit).is_err() {
                break;
            }
        })
        .map_err(|error| io::Error::other(format!("start portal owner audit: {error}")))?;
    let mut state = ServiceState::default();
    loop {
        let event = receiver
            .recv()
            .map_err(|_| io::Error::other("all portal service workers stopped"))?;
        match event {
            ServiceEvent::Audit => {
                begin_active_audit(connection, &mut state)?;
                continue;
            }
            ServiceEvent::Dialog { path, notice } => {
                consume_dialog_notice(connection, &mut state, &path, notice)?;
                continue;
            }
            ServiceEvent::Bus(Err(error)) => return Err(error),
            ServiceEvent::Bus(Ok(frame)) => {
                consume_bus_frame(connection, settings, &mut state, &sender, frame)?;
            }
        }
    }
}

enum ServiceEvent {
    Bus(io::Result<IncomingFrame>),
    Audit,
    Dialog {
        path: String,
        notice: wayland_dialog::Notice,
    },
}

#[derive(Debug)]
struct PendingOpen {
    endian: Endian,
    call_serial: u32,
    owner: String,
    wants_reply: bool,
    token: String,
    title: String,
    parent_handle: String,
    mode: file_chooser::Mode,
    accept_label: Option<String>,
    filter: Option<file_chooser::FileFilter>,
}

#[derive(Debug)]
struct ActiveDialog {
    owner: String,
    endian: Endian,
    cancel: Option<UnixStream>,
    presented: bool,
    cancelled: bool,
    filter: Option<file_chooser::FileFilter>,
}

#[derive(Default)]
struct ServiceState {
    handles: Handles,
    pending: BTreeMap<u32, PendingOpen>,
    pending_audits: BTreeMap<u32, String>,
    active: BTreeMap<String, ActiveDialog>,
    connector: Arc<AtomicBool>,
}

fn consume_bus_frame(
    connection: &mut Connection,
    settings: &Settings,
    state: &mut ServiceState,
    events: &mpsc::SyncSender<ServiceEvent>,
    frame: IncomingFrame,
) -> io::Result<()> {
    let frame = match frame {
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
            return Ok(());
        }
        IncomingFrame::Oversized { call: None, .. } => return Ok(()),
    };
    let (incoming, consumed) = message::decode(&frame, 0).map_err(message_error)?;
    if consumed != frame.len() {
        return Err(io::Error::other("a portal call carried trailing bytes"));
    }
    if consume_active_audit_reply(connection, state, &incoming)?
        || consume_identity_reply(connection, state, events, &incoming)?
    {
        return Ok(());
    }
    if is_open_file_call(&incoming) {
        begin_open_file(connection, state, &incoming)?;
        return Ok(());
    }
    let departed = owner_departure(&incoming)?.map(str::to_string);
    let outbound = dispatch(
        &incoming,
        settings,
        &mut state.handles,
        &mut connection.serial,
    )?;
    for frame in &outbound.frames {
        connection.write_frame(frame)?;
    }
    if let Some(path) = outbound.retire {
        if !state.handles.retire(&path) {
            return Err(io::Error::other(
                "a completed portal handle disappeared before retirement",
            ));
        }
        cancel_dialog(state, &path)?;
    }
    if let Some(owner) = departed {
        state.pending.retain(|_, pending| pending.owner != owner);
        let paths = state
            .active
            .iter()
            .filter_map(|(path, active)| (active.owner == owner).then_some(path.clone()))
            .collect::<Vec<_>>();
        for path in paths {
            cancel_dialog(state, &path)?;
        }
    }
    Ok(())
}

fn is_open_file_call(call: &Message<'_>) -> bool {
    call.kind == MessageType::MethodCall
        && call.fields.path == Some(PORTAL_PATH)
        && call.fields.interface == Some(FILE_CHOOSER_INTERFACE)
        && call.fields.member == Some("OpenFile")
}

fn begin_open_file(
    connection: &mut Connection,
    state: &mut ServiceState,
    call: &Message<'_>,
) -> io::Result<()> {
    let wants_reply = call.flags & message::FLAG_NO_REPLY_EXPECTED == 0;
    let owner = call
        .fields
        .sender
        .ok_or_else(|| io::Error::other("the broker forwarded OpenFile with no sender"))?;
    let parsed = match parse_open_file(call) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!(
                "td-portal: refused FileChooser OpenFile: {}: {}",
                error.name, error.text
            );
            if wants_reply {
                let reply_serial = connection.next_serial()?;
                let frame = method_error(
                    call.endian,
                    reply_serial,
                    call.serial,
                    owner,
                    error.name,
                    error.text,
                )?;
                connection.write_frame(&frame)?;
            }
            return Ok(());
        }
    };
    if state.pending.len() >= MAX_PENDING_OPENS
        || pending_open_count_for_owner(state, owner) >= MAX_PENDING_OPENS_PER_OWNER
    {
        if wants_reply {
            let reply_serial = connection.next_serial()?;
            let frame = method_error(
                call.endian,
                reply_serial,
                call.serial,
                owner,
                "org.freedesktop.DBus.Error.LimitsExceeded",
                "too many FileChooser caller identities are pending",
            )?;
            connection.write_frame(&frame)?;
        }
        return Ok(());
    }
    let query_serial = connection.next_serial()?;
    let query = credentials_query(query_serial, owner)?;
    state.pending.insert(
        query_serial,
        PendingOpen {
            endian: call.endian,
            call_serial: call.serial,
            owner: owner.to_string(),
            wants_reply,
            token: parsed.token,
            title: parsed.title,
            parent_handle: parsed.parent_handle,
            mode: parsed.mode,
            accept_label: parsed.accept_label,
            filter: parsed.filter,
        },
    );
    connection.write_frame(&query)
}

fn pending_identity_count(state: &ServiceState) -> usize {
    state
        .pending
        .len()
        .saturating_add(state.pending_audits.len())
}

fn pending_identity_count_for_owner(state: &ServiceState, owner: &str) -> usize {
    pending_open_count_for_owner(state, owner).saturating_add(
        state
            .pending_audits
            .values()
            .filter_map(|path| state.active.get(path))
            .filter(|active| active.owner == owner)
            .count(),
    )
}

fn pending_open_count_for_owner(state: &ServiceState, owner: &str) -> usize {
    state
        .pending
        .values()
        .filter(|pending| pending.owner == owner)
        .count()
}

fn credentials_query(serial: u32, owner: &str) -> io::Result<Vec<u8>> {
    message::Builder::method_call(
        Endian::Little,
        BUS_PATH,
        Some(BUS_NAME),
        "GetConnectionCredentials",
    )
    .destination(BUS_NAME)
    .serial(serial)
    .body("s", |writer| writer.string(owner))
    .map_err(wire_error)?
    .encode()
    .map_err(message_error)
}

fn begin_active_audit(connection: &mut Connection, state: &mut ServiceState) -> io::Result<()> {
    if !state.pending_audits.is_empty() {
        return Ok(());
    }
    let Some((path, active)) = state.active.iter().find(|(_, active)| !active.cancelled) else {
        return Ok(());
    };
    let path = path.clone();
    let owner = active.owner.clone();
    if pending_identity_count(state) >= MAX_PENDING_IDENTITIES
        || pending_identity_count_for_owner(state, &owner) >= MAX_PENDING_IDENTITIES_PER_OWNER
    {
        return Ok(());
    }
    let serial = connection.next_serial()?;
    let query = credentials_query(serial, &owner)?;
    state.pending_audits.insert(serial, path);
    connection.write_frame(&query)
}

fn consume_active_audit_reply(
    connection: &mut Connection,
    state: &mut ServiceState,
    reply: &Message<'_>,
) -> io::Result<bool> {
    let Some(reply_serial) = reply.fields.reply_serial else {
        return Ok(false);
    };
    if !state.pending_audits.contains_key(&reply_serial) {
        return Ok(false);
    }
    if !matches!(reply.kind, MessageType::MethodReturn | MessageType::Error)
        || reply.fields.sender != Some(BUS_NAME)
        || reply.fields.destination != connection.unique.as_deref()
    {
        return Ok(true);
    }
    let path = state
        .pending_audits
        .remove(&reply_serial)
        .ok_or_else(|| io::Error::other("an active FileChooser audit disappeared"))?;
    let live_firefox = reply.kind == MessageType::MethodReturn
        && credentials_app_id(reply)
            .map(|app_id| app_id.as_deref() == Some("firefox"))
            .unwrap_or(false);
    if live_firefox {
        return Ok(true);
    }
    let response = state
        .active
        .get(&path)
        .filter(|active| !active.cancelled)
        .map(|active| (active.endian, active.owner.clone()));
    if state.handles.retire(&path) {
        eprintln!("td-portal: FileChooser {path} lost its authenticated owner");
        if let Some((endian, owner)) = response {
            let outcome = Err("FileChooser lost its authenticated owner".to_string());
            let serial = connection.next_serial()?;
            let frame = file_chooser_response(endian, serial, &path, &owner, &outcome, None)?;
            connection.write_frame(&frame)?;
        }
    }
    cancel_dialog(state, &path)?;
    Ok(true)
}

fn consume_identity_reply(
    connection: &mut Connection,
    state: &mut ServiceState,
    events: &mpsc::SyncSender<ServiceEvent>,
    reply: &Message<'_>,
) -> io::Result<bool> {
    let Some(reply_serial) = reply.fields.reply_serial else {
        return Ok(false);
    };
    if !state.pending.contains_key(&reply_serial) {
        return Ok(false);
    }
    if !matches!(reply.kind, MessageType::MethodReturn | MessageType::Error)
        || reply.fields.sender != Some(BUS_NAME)
        || reply.fields.destination != connection.unique.as_deref()
    {
        return Ok(true);
    }
    let pending = state
        .pending
        .remove(&reply_serial)
        .ok_or_else(|| io::Error::other("a pending FileChooser identity disappeared"))?;
    if reply.kind == MessageType::Error {
        return refuse_open_file_identity(
            connection,
            &pending,
            "the broker refused caller identity",
        )
        .map(|()| true);
    }
    let app_id = match credentials_app_id(reply) {
        Ok(app_id) => app_id,
        Err(error) => {
            eprintln!("td-portal: refused malformed FileChooser identity: {error}");
            return refuse_open_file_identity(
                connection,
                &pending,
                "the broker returned an invalid caller identity",
            )
            .map(|()| true);
        }
    };
    if app_id.as_deref() != Some("firefox") {
        return refuse_open_file_identity(
            connection,
            &pending,
            "FileChooser currently admits only the authenticated Firefox grant",
        )
        .map(|()| true);
    }
    if state.active.len() >= MAX_ACTIVE_FILE_CHOOSERS {
        return refuse_open_file_limit(connection, &pending).map(|()| true);
    }
    let path = match state
        .handles
        .reserve(HandleKind::Request, &pending.owner, &pending.token)
    {
        Ok(path) => path,
        Err(error) => {
            return refuse_open_file_reservation(connection, &pending, error).map(|()| true)
        }
    };
    if pending.wants_reply {
        let serial = connection.next_serial()?;
        let frame = method_return(
            pending.endian,
            serial,
            pending.call_serial,
            &pending.owner,
            "o",
            |writer| writer.object_path(&path),
        )?;
        connection.write_frame(&frame)?;
    }
    state.active.insert(
        path.clone(),
        ActiveDialog {
            owner: pending.owner.clone(),
            endian: pending.endian,
            cancel: None,
            presented: false,
            cancelled: false,
            filter: pending.filter.clone(),
        },
    );
    let config = wayland_dialog::DialogConfig {
        socket: PathBuf::from(FILE_CHOOSER_SOCKET),
        runtime_directory: PathBuf::from(FILE_CHOOSER_RUNTIME),
        title: pending.title,
        parent_handle: pending.parent_handle,
        app_id: app_id.unwrap_or_default(),
        host_root: PathBuf::from(FIREFOX_HOST_DOWNLOADS),
        guest_root: PathBuf::from(FIREFOX_GUEST_DOWNLOADS),
        mode: pending.mode,
        accept_label: pending.accept_label,
        filter: pending.filter,
        connector: state.connector.clone(),
    };
    let event_sender = events.clone();
    let event_path = path.clone();
    if let Err(error) = wayland_dialog::spawn(config, move |notice| {
        event_sender
            .send(ServiceEvent::Dialog {
                path: event_path.clone(),
                notice,
            })
            .map_err(|_| "portal service stopped before its dialog worker".to_string())
    }) {
        state.active.remove(&path);
        if !state.handles.retire(&path) {
            return Err(io::Error::other(
                "failed FileChooser worker lost its Request handle",
            ));
        }
        let serial = connection.next_serial()?;
        connection.write_frame(&file_chooser_response(
            pending.endian,
            serial,
            &path,
            &pending.owner,
            &Err(error),
            None,
        )?)?;
    }
    Ok(true)
}

fn consume_dialog_notice(
    connection: &mut Connection,
    state: &mut ServiceState,
    path: &str,
    notice: wayland_dialog::Notice,
) -> io::Result<()> {
    match notice {
        wayland_dialog::Notice::Connected(stream) => {
            if let Some(active) = state.active.get_mut(path) {
                if active.cancelled {
                    shutdown_dialog_stream(&stream)?;
                    return Ok(());
                }
                if active.cancel.replace(stream).is_some() {
                    return Err(io::Error::other(
                        "a FileChooser worker sent two cancellation handles",
                    ));
                }
            } else {
                shutdown_dialog_stream(&stream)?;
            }
        }
        wayland_dialog::Notice::Presented {
            width,
            height,
            checksum,
        } => {
            let Some(active) = state.active.get_mut(path) else {
                return Ok(());
            };
            if active.cancelled {
                return Ok(());
            }
            if active.presented {
                return Err(io::Error::other(
                    "a FileChooser worker announced its first frame twice",
                ));
            }
            active.presented = true;
            println!(
                "TD-PORTAL-FILE-CHOOSER-PRESENTED path={path} size={width}x{height} checksum={checksum:016x}"
            );
            io::stdout().flush()?;
        }
        wayland_dialog::Notice::Completed(outcome) => {
            let Some(active) = state.active.remove(path) else {
                return Ok(());
            };
            state
                .pending_audits
                .retain(|_, audited_path| audited_path != path);
            if active.cancelled {
                return Ok(());
            }
            let outcome = require_presented_outcome(active.presented, outcome);
            if !state.handles.retire(path) {
                return Err(io::Error::other(
                    "a completed FileChooser lost its Request handle",
                ));
            }
            let serial = connection.next_serial()?;
            let (frame, record) = file_chooser_completion(
                active.endian,
                serial,
                path,
                &active.owner,
                &outcome,
                active.filter.as_ref(),
            )?;
            connection.write_frame(&frame)?;
            println!("{record}");
            io::stdout().flush()?;
        }
    }
    Ok(())
}

fn require_presented_outcome(
    presented: bool,
    outcome: Result<file_chooser::Outcome, String>,
) -> Result<file_chooser::Outcome, String> {
    if !presented && outcome.is_ok() {
        Err("FileChooser completed before a presented modal frame".into())
    } else {
        outcome
    }
}

fn cancel_dialog(state: &mut ServiceState, path: &str) -> io::Result<()> {
    state
        .pending_audits
        .retain(|_, audited_path| audited_path != path);
    let Some(active) = state.active.get_mut(path) else {
        return Ok(());
    };
    if active.cancelled {
        return Ok(());
    }
    active.cancelled = true;
    if let Some(stream) = active.cancel.take() {
        shutdown_dialog_stream(&stream)?;
    } else {
        eprintln!(
            "FileChooser {path} was cancelled before its blocking AF_UNIX connect returned; retaining the sole dialog slot"
        );
    }
    Ok(())
}

fn shutdown_dialog_stream(stream: &UnixStream) -> io::Result<()> {
    match stream.shutdown(Shutdown::Both) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotConnected => Ok(()),
        Err(error) => Err(error),
    }
}

#[derive(Debug, Default)]
struct Outbound {
    frames: Vec<Vec<u8>>,
    retire: Option<String>,
}

impl Outbound {
    fn reply(wants_reply: bool, frame: Vec<u8>) -> Self {
        let mut frames = Vec::with_capacity(1);
        if wants_reply {
            frames.push(frame);
        }
        Self {
            frames,
            retire: None,
        }
    }
}

fn dispatch(
    call: &Message<'_>,
    settings: &Settings,
    handles: &mut Handles,
    serials: &mut u32,
) -> io::Result<Outbound> {
    if call.kind == MessageType::Signal {
        consume_owner_departure(call, handles)?;
        return Ok(Outbound::default());
    }
    if call.kind != MessageType::MethodCall {
        return Ok(Outbound::default());
    }
    let wants_reply = call.flags & message::FLAG_NO_REPLY_EXPECTED == 0;
    let sender = call
        .fields
        .sender
        .ok_or_else(|| io::Error::other("the broker forwarded a call with no sender"))?;
    if let (Some(path), Some(interface), Some("Close")) =
        (call.fields.path, call.fields.interface, call.fields.member)
    {
        if matches!(
            interface,
            handles::REQUEST_INTERFACE | handles::SESSION_INTERFACE
        ) {
            return close_handle(call, handles, serials, sender, path, interface, wants_reply);
        }
    }
    let serial = next_serial_value(serials)?;
    let answer = match (call.fields.path, call.fields.interface, call.fields.member) {
        (Some(PORTAL_PATH), Some(SETTINGS_INTERFACE), Some("ReadAll")) => {
            read_all_reply(call, settings, serial, sender)
        }
        (Some(PORTAL_PATH), Some(SETTINGS_INTERFACE), Some("Read")) => {
            read_reply(call, settings, serial, sender)
        }
        (Some(PORTAL_PATH), Some(BACKGROUND_INTERFACE), Some("RequestBackground")) => {
            return request_background(call, handles, serials, serial, sender, wants_reply)
        }
        (Some(PORTAL_PATH), Some(FILE_CHOOSER_INTERFACE), Some("SaveFile" | "SaveFiles")) => {
            if exact_signature(call, "ssa{sv}") {
                method_error(
                    call.endian,
                    serial,
                    call.serial,
                    sender,
                    UNSUPPORTED_OPEN,
                    "td FileChooser does not yet implement saving",
                )
            } else {
                invalid_args(
                    call,
                    serial,
                    sender,
                    "SaveFile and SaveFiles take parent, title, and options",
                )
            }
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
        (Some(path), Some(PROPERTIES_INTERFACE), Some("Get")) => {
            handle_property_get(call, handles, serial, sender, path)
        }
        (Some(path), Some(PROPERTIES_INTERFACE), Some("GetAll")) => {
            handle_property_get_all(call, handles, serial, sender, path)
        }
        (Some(path), Some(PROPERTIES_INTERFACE), Some("Set")) => {
            handle_property_set(call, handles, serial, sender, path)
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
        (Some(path), Some(INTROSPECT_INTERFACE), Some("Introspect")) => {
            introspect_handle(call, handles, serial, sender, path)
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
            UNKNOWN_METHOD_ERROR,
            UNKNOWN_METHOD_TEXT,
        ),
    }?;
    Ok(Outbound::reply(wants_reply, answer))
}

fn consume_owner_departure(call: &Message<'_>, handles: &mut Handles) -> io::Result<()> {
    if let Some(owner) = owner_departure(call)? {
        handles.remove_owner(owner);
    }
    Ok(())
}

fn owner_departure<'a>(call: &'a Message<'a>) -> io::Result<Option<&'a str>> {
    if call.fields.sender != Some(BUS_NAME)
        || call.fields.path != Some(BUS_PATH)
        || call.fields.interface != Some(BUS_NAME)
        || call.fields.member != Some("NameOwnerChanged")
    {
        return Ok(None);
    }
    if !exact_signature(call, "sss") {
        return Err(io::Error::other(
            "the broker sent a malformed NameOwnerChanged signal",
        ));
    }
    let [Value::Str(name), Value::Str(old), Value::Str(new)] = call.args() else {
        return Err(io::Error::other(
            "the broker sent a malformed NameOwnerChanged body",
        ));
    };
    if new.is_empty() && name == old && name::valid_unique_name(name) {
        return Ok(Some(name));
    }
    Ok(None)
}

fn introspect_handle(
    call: &Message<'_>,
    handles: &Handles,
    serial: u32,
    sender: &str,
    path: &str,
) -> io::Result<Vec<u8>> {
    if !exact_signature(call, "") {
        return invalid_args(call, serial, sender, "Introspect takes no arguments");
    }
    let xml = match handles.lookup(path, sender) {
        Ok(HandleKind::Request) => REQUEST_INTROSPECTION_XML,
        Ok(HandleKind::Session) => SESSION_INTROSPECTION_XML,
        Err(LookupError::Missing | LookupError::Foreign) => {
            return unknown_object(call, serial, sender, "that portal handle does not exist")
        }
    };
    method_return(call.endian, serial, call.serial, sender, "s", |writer| {
        writer.string(xml)
    })
}

fn handle_property_get(
    call: &Message<'_>,
    handles: &Handles,
    serial: u32,
    sender: &str,
    path: &str,
) -> io::Result<Vec<u8>> {
    let Some((interface, property)) = two_strings(call) else {
        return invalid_args(
            call,
            serial,
            sender,
            "Get takes interface and property strings",
        );
    };
    let kind = match handle_kind(call, handles, serial, sender, path)? {
        Ok(kind) => kind,
        Err(frame) => return Ok(frame),
    };
    if !known_handle_interface(kind, interface) {
        return unknown_interface(call, serial, sender);
    }
    if kind != HandleKind::Session
        || property != "version"
        || !matches!(interface, "" | handles::SESSION_INTERFACE)
    {
        return unknown_property(call, serial, sender);
    }
    method_return(call.endian, serial, call.serial, sender, "v", |writer| {
        writer.variant("u", |writer| {
            writer.uint32(SESSION_VERSION);
            Ok(())
        })
    })
}

fn handle_property_get_all(
    call: &Message<'_>,
    handles: &Handles,
    serial: u32,
    sender: &str,
    path: &str,
) -> io::Result<Vec<u8>> {
    if !exact_signature(call, "s") {
        return invalid_args(call, serial, sender, "GetAll takes one interface string");
    }
    let Some(interface) = call.args().first().and_then(Value::as_str) else {
        return invalid_args(call, serial, sender, "GetAll takes one interface string");
    };
    let kind = match handle_kind(call, handles, serial, sender, path)? {
        Ok(kind) => kind,
        Err(frame) => return Ok(frame),
    };
    if !known_handle_interface(kind, interface) {
        return unknown_interface(call, serial, sender);
    }
    method_return(
        call.endian,
        serial,
        call.serial,
        sender,
        "a{sv}",
        |writer| {
            writer.array("{sv}", |writer| {
                if kind == HandleKind::Session
                    && matches!(interface, "" | handles::SESSION_INTERFACE)
                {
                    writer.dict_entry(|writer| {
                        writer.string("version")?;
                        writer.variant("u", |writer| {
                            writer.uint32(SESSION_VERSION);
                            Ok(())
                        })
                    })?;
                }
                Ok(())
            })
        },
    )
}

fn handle_property_set(
    call: &Message<'_>,
    handles: &Handles,
    serial: u32,
    sender: &str,
    path: &str,
) -> io::Result<Vec<u8>> {
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
    let kind = match handle_kind(call, handles, serial, sender, path)? {
        Ok(kind) => kind,
        Err(frame) => return Ok(frame),
    };
    if !known_handle_interface(kind, interface) {
        return unknown_interface(call, serial, sender);
    }
    if kind != HandleKind::Session
        || *property != "version"
        || !matches!(*interface, "" | handles::SESSION_INTERFACE)
    {
        return unknown_property(call, serial, sender);
    }
    method_error(
        call.endian,
        serial,
        call.serial,
        sender,
        "org.freedesktop.DBus.Error.PropertyReadOnly",
        "the Session version is read-only",
    )
}

fn handle_kind(
    call: &Message<'_>,
    handles: &Handles,
    serial: u32,
    sender: &str,
    path: &str,
) -> io::Result<Result<HandleKind, Vec<u8>>> {
    Ok(match handles.lookup(path, sender) {
        Ok(kind) => Ok(kind),
        Err(LookupError::Missing) => Err(unknown_object(
            call,
            serial,
            sender,
            "that portal handle does not exist",
        )?),
        Err(LookupError::Foreign) => Err(unknown_object(
            call,
            serial,
            sender,
            "that portal handle does not exist",
        )?),
    })
}

fn handle_interface(kind: HandleKind) -> &'static str {
    match kind {
        HandleKind::Request => handles::REQUEST_INTERFACE,
        HandleKind::Session => handles::SESSION_INTERFACE,
    }
}

fn known_handle_interface(kind: HandleKind, interface: &str) -> bool {
    interface.is_empty()
        || interface == handle_interface(kind)
        || matches!(
            interface,
            PROPERTIES_INTERFACE | INTROSPECT_INTERFACE | PEER_INTERFACE
        )
}

fn close_handle(
    call: &Message<'_>,
    handles: &Handles,
    serials: &mut u32,
    sender: &str,
    path: &str,
    interface: &str,
    wants_reply: bool,
) -> io::Result<Outbound> {
    let serial = next_serial_value(serials)?;
    if !exact_signature(call, "") {
        return invalid_args(call, serial, sender, "Close takes no arguments")
            .map(|frame| Outbound::reply(wants_reply, frame));
    }
    let expected = if interface == handles::REQUEST_INTERFACE {
        HandleKind::Request
    } else {
        HandleKind::Session
    };
    let kind = match handles.lookup(path, sender) {
        Ok(kind) => kind,
        Err(LookupError::Missing | LookupError::Foreign) => {
            return unknown_object(call, serial, sender, "that portal handle does not exist")
                .map(|frame| Outbound::reply(wants_reply, frame))
        }
    };
    if kind != expected {
        return unknown_interface(call, serial, sender)
            .map(|frame| Outbound::reply(wants_reply, frame));
    }
    let mut frames = Vec::with_capacity(2);
    if wants_reply {
        frames.push(method_return(
            call.endian,
            serial,
            call.serial,
            sender,
            "",
            |_| Ok(()),
        )?);
    }
    Ok(Outbound {
        frames,
        retire: Some(path.to_string()),
    })
}

fn exact_signature(call: &Message<'_>, signature: &str) -> bool {
    call.fields.signature.unwrap_or("") == signature
}

#[derive(Debug)]
struct ParsedOpen {
    token: String,
    title: String,
    parent_handle: String,
    mode: file_chooser::Mode,
    accept_label: Option<String>,
    filter: Option<file_chooser::FileFilter>,
}

#[derive(Clone, Copy, Debug)]
struct PortalCallError {
    name: &'static str,
    text: &'static str,
}

const INVALID_OPEN: &str = "org.freedesktop.DBus.Error.InvalidArgs";
const UNSUPPORTED_OPEN: &str = "org.freedesktop.portal.Error.NotSupported";

fn open_error(name: &'static str, text: &'static str) -> PortalCallError {
    PortalCallError { name, text }
}

fn parse_open_file(call: &Message<'_>) -> Result<ParsedOpen, PortalCallError> {
    if !exact_signature(call, "ssa{sv}") {
        return Err(open_error(
            INVALID_OPEN,
            "OpenFile takes parent, title, and options",
        ));
    }
    let [Value::Str(parent), Value::Str(title), Value::Array(options)] = call.args() else {
        return Err(open_error(
            INVALID_OPEN,
            "OpenFile arguments have the wrong shape",
        ));
    };
    if title.len() > MAX_FILE_CHOOSER_TITLE_BYTES || title.chars().any(char::is_control) {
        return Err(open_error(
            INVALID_OPEN,
            "OpenFile title is outside the 256-byte text bound",
        ));
    }
    let parent_handle = if parent.is_empty() {
        String::new()
    } else if let Some(handle) = parent.strip_prefix("wayland:") {
        if handle.is_empty()
            || handle.len() > 128
            || handle.bytes().any(|byte| !byte.is_ascii_graphic())
        {
            return Err(open_error(
                INVALID_OPEN,
                "OpenFile has a malformed Wayland parent handle",
            ));
        }
        handle.to_string()
    } else {
        return Err(open_error(
            UNSUPPORTED_OPEN,
            "td FileChooser supports only Wayland parent identifiers",
        ));
    };
    let entries = options
        .values(MAX_PORTAL_OPTIONS)
        .map_err(|_| open_error(INVALID_OPEN, "OpenFile has too many options"))?;
    let mut token = None;
    let mut multiple = None;
    let mut directory = None;
    let mut modal = None;
    let mut accept_label = None;
    let mut filters = None;
    let mut current_filter = None;
    let mut current_folder = false;
    for entry in entries {
        let pair = entry
            .as_seq()
            .ok_or_else(|| open_error(INVALID_OPEN, "an OpenFile option is not an entry"))?
            .values(2)
            .map_err(|_| open_error(INVALID_OPEN, "an OpenFile option is malformed"))?;
        let [Value::Str(key), Value::Variant(value)] = pair.as_slice() else {
            return Err(open_error(
                INVALID_OPEN,
                "an OpenFile option has the wrong shape",
            ));
        };
        match *key {
            "handle_token" => {
                if token.is_some() {
                    return Err(open_error(INVALID_OPEN, "handle_token appears twice"));
                }
                let text = variant_string(*value, "handle_token")?;
                if !handles::valid_token(text) {
                    return Err(open_error(
                        INVALID_OPEN,
                        "handle_token is not a valid object-path element",
                    ));
                }
                token = Some(text.to_string());
            }
            "multiple" => {
                if multiple
                    .replace(variant_bool(*value, "multiple")?)
                    .is_some()
                {
                    return Err(open_error(INVALID_OPEN, "multiple appears twice"));
                }
            }
            "directory" => {
                if directory
                    .replace(variant_bool(*value, "directory")?)
                    .is_some()
                {
                    return Err(open_error(INVALID_OPEN, "directory appears twice"));
                }
            }
            "modal" => {
                if modal.replace(variant_bool(*value, "modal")?).is_some() {
                    return Err(open_error(INVALID_OPEN, "modal appears twice"));
                }
            }
            "accept_label" => {
                if accept_label.is_some() {
                    return Err(open_error(INVALID_OPEN, "accept_label appears twice"));
                }
                let label = variant_string(*value, "accept_label")?;
                if label.is_empty()
                    || label.len() > file_chooser::MAX_ACCEPT_LABEL_BYTES
                    || label.chars().any(char::is_control)
                {
                    return Err(open_error(
                        INVALID_OPEN,
                        "accept_label is outside the 64-byte text bound",
                    ));
                }
                accept_label = Some(label.to_string());
            }
            "filters" => {
                if filters.is_some() {
                    return Err(open_error(INVALID_OPEN, "filters appears twice"));
                }
                filters = Some(parse_filters(*value)?);
            }
            "current_filter" => {
                if current_filter.is_some() {
                    return Err(open_error(INVALID_OPEN, "current_filter appears twice"));
                }
                current_filter = Some(parse_current_filter(*value)?);
            }
            "current_folder" => {
                if current_folder {
                    return Err(open_error(INVALID_OPEN, "current_folder appears twice"));
                }
                current_folder = true;
                validate_current_folder(*value)?;
            }
            "choices" => {
                return Err(open_error(
                    UNSUPPORTED_OPEN,
                    "td FileChooser does not yet implement choices",
                ));
            }
            _ => {}
        }
    }
    let filter = select_filter(filters, current_filter)?;
    if modal == Some(false) {
        return Err(open_error(
            UNSUPPORTED_OPEN,
            "td FileChooser currently serves modal requests only",
        ));
    }
    let multiple = multiple.unwrap_or(false);
    let directory = directory.unwrap_or(false);
    if multiple && directory {
        return Err(open_error(
            UNSUPPORTED_OPEN,
            "td FileChooser does not yet select multiple directories",
        ));
    }
    Ok(ParsedOpen {
        token: token.unwrap_or_else(|| format!("td_{}", call.serial)),
        title: title.to_string(),
        parent_handle,
        mode: if directory {
            file_chooser::Mode::OpenDirectory
        } else {
            file_chooser::Mode::OpenFile { multiple }
        },
        accept_label,
        filter,
    })
}

fn variant_string<'a>(
    value: wire::Seq<'a>,
    option: &'static str,
) -> Result<&'a str, PortalCallError> {
    if value.signature() != "s" {
        return Err(open_error(
            INVALID_OPEN,
            "an OpenFile string option is mistyped",
        ));
    }
    let values = value
        .values(1)
        .map_err(|_| open_error(INVALID_OPEN, "an OpenFile string option is malformed"))?;
    match values.as_slice() {
        [Value::Str(text)] => Ok(*text),
        _ => Err(open_error(INVALID_OPEN, option)),
    }
}

fn variant_bool(value: wire::Seq<'_>, option: &'static str) -> Result<bool, PortalCallError> {
    if value.signature() != "b" {
        return Err(open_error(
            INVALID_OPEN,
            "an OpenFile boolean option is mistyped",
        ));
    }
    let values = value
        .values(1)
        .map_err(|_| open_error(INVALID_OPEN, "an OpenFile boolean option is malformed"))?;
    match values.as_slice() {
        [Value::Bool(value)] => Ok(*value),
        _ => Err(open_error(INVALID_OPEN, option)),
    }
}

#[derive(Debug, Eq, PartialEq)]
struct ParsedFileFilter {
    label: String,
    rules: Vec<(u32, String)>,
}

impl ParsedFileFilter {
    fn selected_all_files(&self) -> Result<file_chooser::FileFilter, PortalCallError> {
        let [(kind, pattern)] = self.rules.as_slice() else {
            return Err(open_error(
                UNSUPPORTED_OPEN,
                "td FileChooser supports an all-files current filter",
            ));
        };
        if *kind != 0 || !matches!(pattern.as_str(), "*" | "*.*") {
            return Err(open_error(
                UNSUPPORTED_OPEN,
                "td FileChooser supports an all-files current filter",
            ));
        }
        file_chooser::FileFilter::all_files(&self.label, pattern)
            .map_err(|_| open_error(INVALID_OPEN, "the all-files filter is malformed"))
    }
}

fn parse_filters(value: wire::Seq<'_>) -> Result<Vec<ParsedFileFilter>, PortalCallError> {
    if value.signature() != "a(sa(us))" {
        return Err(open_error(INVALID_OPEN, "filters has the wrong type"));
    }
    let values = value
        .values(1)
        .map_err(|_| open_error(INVALID_OPEN, "filters is malformed"))?;
    let Some(Value::Array(filters)) = values.first() else {
        return Err(open_error(INVALID_OPEN, "filters has the wrong shape"));
    };
    let filters = filters
        .values(32)
        .map_err(|_| open_error(INVALID_OPEN, "filters exceeds 32 entries"))?;
    let mut parsed = Vec::with_capacity(filters.len());
    for filter in filters {
        parsed.push(parse_filter(filter)?);
    }
    Ok(parsed)
}

fn parse_current_filter(value: wire::Seq<'_>) -> Result<ParsedFileFilter, PortalCallError> {
    if value.signature() != "(sa(us))" {
        return Err(open_error(
            INVALID_OPEN,
            "current_filter has the wrong type",
        ));
    }
    let values = value
        .values(1)
        .map_err(|_| open_error(INVALID_OPEN, "current_filter is malformed"))?;
    let Some(filter) = values.first().copied() else {
        return Err(open_error(INVALID_OPEN, "current_filter is empty"));
    };
    parse_filter(filter)
}

fn parse_filter(filter: Value<'_>) -> Result<ParsedFileFilter, PortalCallError> {
    let fields = filter
        .as_seq()
        .ok_or_else(|| open_error(INVALID_OPEN, "a file filter is not a structure"))?
        .values(2)
        .map_err(|_| open_error(INVALID_OPEN, "a file filter is malformed"))?;
    let [Value::Str(label), Value::Array(rules)] = fields.as_slice() else {
        return Err(open_error(
            INVALID_OPEN,
            "a file filter has the wrong shape",
        ));
    };
    if label.is_empty() || label.len() > 256 || label.chars().any(char::is_control) {
        return Err(open_error(
            INVALID_OPEN,
            "a file filter label is outside the 256-byte text bound",
        ));
    }
    let rules = rules
        .values(32)
        .map_err(|_| open_error(INVALID_OPEN, "a file filter exceeds 32 rules"))?;
    let mut parsed_rules = Vec::with_capacity(rules.len());
    for rule in rules {
        let pair = rule
            .as_seq()
            .ok_or_else(|| open_error(INVALID_OPEN, "a file filter rule is not a structure"))?
            .values(2)
            .map_err(|_| open_error(INVALID_OPEN, "a file filter rule is malformed"))?;
        let [Value::Uint32(kind), Value::Str(pattern)] = pair.as_slice() else {
            return Err(open_error(
                INVALID_OPEN,
                "a file filter rule has the wrong shape",
            ));
        };
        if !matches!(*kind, 0 | 1)
            || pattern.is_empty()
            || pattern.len() > 256
            || pattern.chars().any(char::is_control)
        {
            return Err(open_error(
                INVALID_OPEN,
                "a file filter rule is outside its closed bound",
            ));
        }
        parsed_rules.push((*kind, (*pattern).to_string()));
    }
    Ok(ParsedFileFilter {
        label: (*label).to_string(),
        rules: parsed_rules,
    })
}

fn select_filter(
    filters: Option<Vec<ParsedFileFilter>>,
    current: Option<ParsedFileFilter>,
) -> Result<Option<file_chooser::FileFilter>, PortalCallError> {
    let filters = filters.unwrap_or_default();
    if filters.is_empty() {
        return if current.is_some() {
            Err(open_error(
                UNSUPPORTED_OPEN,
                "current_filter requires a matching filters entry",
            ))
        } else {
            Ok(None)
        };
    }
    let selected = match current {
        Some(current) => {
            if !filters.iter().any(|filter| filter == &current) {
                return Err(open_error(
                    UNSUPPORTED_OPEN,
                    "current_filter does not select an advertised filter",
                ));
            }
            current
        }
        None if filters.len() == 1 => filters
            .into_iter()
            .next()
            .ok_or_else(|| open_error(INVALID_OPEN, "the file filter list disappeared"))?,
        None => {
            return Err(open_error(
                UNSUPPORTED_OPEN,
                "multiple filters require an explicit current_filter",
            ));
        }
    };
    selected.selected_all_files().map(Some)
}

fn validate_current_folder(value: wire::Seq<'_>) -> Result<(), PortalCallError> {
    if value.signature() != "ay" {
        return Err(open_error(
            INVALID_OPEN,
            "current_folder has the wrong type",
        ));
    }
    let values = value
        .values(1)
        .map_err(|_| open_error(INVALID_OPEN, "current_folder is malformed"))?;
    let Some(Value::Array(bytes)) = values.first() else {
        return Err(open_error(
            INVALID_OPEN,
            "current_folder has the wrong shape",
        ));
    };
    let bytes = bytes
        .as_bytes()
        .ok_or_else(|| open_error(INVALID_OPEN, "current_folder is not bytes"))?;
    if bytes.len() > file_chooser::MAX_PATH_BYTES.saturating_add(1)
        || bytes.last().copied() != Some(0)
        || bytes
            .get(..bytes.len().saturating_sub(1))
            .is_some_and(|path| path.contains(&0))
    {
        return Err(open_error(
            INVALID_OPEN,
            "current_folder is not one bounded NUL-terminated path",
        ));
    }
    Ok(())
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

fn request_background(
    call: &Message<'_>,
    handles: &mut Handles,
    serials: &mut u32,
    reply_serial: u32,
    sender: &str,
    wants_reply: bool,
) -> io::Result<Outbound> {
    let token = match background_token(call) {
        Ok(Some(token)) => token.to_string(),
        Ok(None) => format!("td_{}", call.serial),
        Err(text) => {
            return invalid_args(call, reply_serial, sender, text)
                .map(|frame| Outbound::reply(wants_reply, frame))
        }
    };
    let path = match handles.reserve(HandleKind::Request, sender, &token) {
        Ok(path) => path,
        Err(ReserveError::InvalidOwner) => {
            return Err(io::Error::other(
                "the broker assigned a sender outside td-busd's unique-name shape",
            ))
        }
        Err(ReserveError::InvalidToken) => {
            return invalid_args(
                call,
                reply_serial,
                sender,
                "handle_token is not a valid object-path element",
            )
            .map(|frame| Outbound::reply(wants_reply, frame))
        }
        Err(ReserveError::Duplicate) => {
            return method_error(
                call.endian,
                reply_serial,
                call.serial,
                sender,
                "org.freedesktop.portal.Error.Exists",
                "that request handle is already active",
            )
            .map(|frame| Outbound::reply(wants_reply, frame))
        }
        Err(ReserveError::OwnerFull | ReserveError::Full) => {
            return method_error(
                call.endian,
                reply_serial,
                call.serial,
                sender,
                "org.freedesktop.DBus.Error.LimitsExceeded",
                "this portal has too many active handles",
            )
            .map(|frame| Outbound::reply(wants_reply, frame))
        }
    };
    let mut frames = Vec::with_capacity(2);
    if wants_reply {
        frames.push(method_return(
            call.endian,
            reply_serial,
            call.serial,
            sender,
            "o",
            |writer| writer.object_path(&path),
        )?);
    }
    let response_serial = next_serial_value(serials)?;
    frames.push(request_response(
        call.endian,
        response_serial,
        &path,
        sender,
        2,
    )?);
    Ok(Outbound {
        frames,
        retire: Some(path),
    })
}

fn background_token<'a>(call: &'a Message<'a>) -> Result<Option<&'a str>, &'static str> {
    if !exact_signature(call, "sa{sv}") {
        return Err("RequestBackground takes a parent string and options dictionary");
    }
    let [Value::Str(_parent), Value::Array(options)] = call.args() else {
        return Err("RequestBackground takes a parent string and options dictionary");
    };
    let entries = options
        .values(MAX_PORTAL_OPTIONS)
        .map_err(|_| "RequestBackground has too many options")?;
    let mut token = None;
    for entry in &entries {
        let pair = entry
            .as_seq()
            .ok_or("a RequestBackground option is not a dictionary entry")?
            .values(2)
            .map_err(|_| "a RequestBackground option is malformed")?;
        let [Value::Str(key), Value::Variant(value)] = pair.as_slice() else {
            return Err("a RequestBackground option has the wrong shape");
        };
        if *key != "handle_token" {
            continue;
        }
        if token.is_some() || value.signature() != "s" {
            return Err("handle_token must appear once as a string");
        }
        let values = value
            .values(1)
            .map_err(|_| "handle_token has a malformed variant")?;
        let Some(Value::Str(text)) = values.first() else {
            return Err("handle_token must be a string");
        };
        if !handles::valid_token(text) {
            return Err("handle_token is not a valid object-path element");
        }
        token = Some(*text);
    }
    Ok(token)
}

fn credentials_app_id(reply: &Message<'_>) -> io::Result<Option<String>> {
    if reply.kind != MessageType::MethodReturn || reply.fields.signature != Some("a{sv}") {
        return Err(io::Error::other(
            "GetConnectionCredentials returned the wrong message shape",
        ));
    }
    let [Value::Array(entries)] = reply.args() else {
        return Err(io::Error::other(
            "GetConnectionCredentials returned no credential dictionary",
        ));
    };
    let mut uid = None;
    let mut app_id = None;
    for entry in entries.values(16).map_err(wire_error)? {
        let pair = entry
            .as_seq()
            .ok_or_else(|| io::Error::other("a credential is not a dictionary entry"))?
            .values(2)
            .map_err(wire_error)?;
        let [Value::Str(key), Value::Variant(value)] = pair.as_slice() else {
            return Err(io::Error::other("a credential has the wrong shape"));
        };
        match *key {
            "UnixUserID" => {
                if uid.is_some() || value.signature() != "u" {
                    return Err(io::Error::other(
                        "GetConnectionCredentials repeated or mistyped UnixUserID",
                    ));
                }
                let values = value.values(1).map_err(wire_error)?;
                let Some(Value::Uint32(value)) = values.first() else {
                    return Err(io::Error::other(
                        "GetConnectionCredentials malformed UnixUserID",
                    ));
                };
                uid = Some(*value);
            }
            "td.AppId" => {
                if app_id.is_some() || value.signature() != "s" {
                    return Err(io::Error::other(
                        "GetConnectionCredentials repeated or mistyped td.AppId",
                    ));
                }
                let values = value.values(1).map_err(wire_error)?;
                let Some(Value::Str(value)) = values.first() else {
                    return Err(io::Error::other(
                        "GetConnectionCredentials malformed td.AppId",
                    ));
                };
                app_id = Some((*value).to_string());
            }
            _ => {}
        }
    }
    if uid != Some(UI_UID) {
        return Err(io::Error::other(format!(
            "GetConnectionCredentials returned uid {uid:?}, expected {UI_UID}"
        )));
    }
    Ok(app_id)
}

fn refuse_open_file_identity(
    connection: &mut Connection,
    pending: &PendingOpen,
    text: &str,
) -> io::Result<()> {
    if !pending.wants_reply {
        return Ok(());
    }
    let serial = connection.next_serial()?;
    let frame = method_error(
        pending.endian,
        serial,
        pending.call_serial,
        &pending.owner,
        "org.freedesktop.portal.Error.NotAllowed",
        text,
    )?;
    connection.write_frame(&frame)
}

fn refuse_open_file_limit(connection: &mut Connection, pending: &PendingOpen) -> io::Result<()> {
    if !pending.wants_reply {
        return Ok(());
    }
    let serial = connection.next_serial()?;
    let frame = method_error(
        pending.endian,
        serial,
        pending.call_serial,
        &pending.owner,
        "org.freedesktop.DBus.Error.LimitsExceeded",
        "another FileChooser dialog is already active",
    )?;
    connection.write_frame(&frame)
}

fn refuse_open_file_reservation(
    connection: &mut Connection,
    pending: &PendingOpen,
    error: ReserveError,
) -> io::Result<()> {
    if matches!(error, ReserveError::InvalidOwner) {
        return Err(io::Error::other(
            "the broker assigned a sender outside td-busd's unique-name shape",
        ));
    }
    if !pending.wants_reply {
        return Ok(());
    }
    let (name, text) = match error {
        ReserveError::Duplicate => (
            "org.freedesktop.portal.Error.Exists",
            "that FileChooser Request already exists",
        ),
        ReserveError::OwnerFull | ReserveError::Full => (
            "org.freedesktop.DBus.Error.LimitsExceeded",
            "this portal has too many active handles",
        ),
        ReserveError::InvalidToken => (
            "org.freedesktop.DBus.Error.InvalidArgs",
            "the FileChooser handle token is invalid",
        ),
        ReserveError::InvalidOwner => return Ok(()),
    };
    let serial = connection.next_serial()?;
    let frame = method_error(
        pending.endian,
        serial,
        pending.call_serial,
        &pending.owner,
        name,
        text,
    )?;
    connection.write_frame(&frame)
}

fn file_chooser_response(
    endian: Endian,
    serial: u32,
    path: &str,
    destination: &str,
    outcome: &Result<file_chooser::Outcome, String>,
    filter: Option<&file_chooser::FileFilter>,
) -> io::Result<Vec<u8>> {
    if let Err(error) = outcome {
        eprintln!("td-portal: FileChooser {path} failed: {error}");
    }
    let response = file_chooser_response_code(outcome);
    message::Builder::signal(endian, path, handles::REQUEST_INTERFACE, "Response")
        .destination(destination)
        .serial(serial)
        .body("ua{sv}", |writer| {
            writer.uint32(response);
            match outcome {
                Ok(file_chooser::Outcome::Accepted(uris)) => {
                    writer.array("{sv}", |writer| {
                        writer.dict_entry(|writer| {
                            writer.string("uris")?;
                            writer.variant("as", |writer| {
                                writer.array("s", |writer| {
                                    for uri in uris {
                                        writer.string(uri)?;
                                    }
                                    Ok(())
                                })
                            })
                        })?;
                        if let Some(filter) = filter {
                            writer.dict_entry(|writer| {
                                writer.string("current_filter")?;
                                writer.variant("(sa(us))", |writer| {
                                    writer.structure(|writer| {
                                        writer.string(filter.label())?;
                                        writer.array("(us)", |writer| {
                                            writer.structure(|writer| {
                                                writer.uint32(0);
                                                writer.string(filter.pattern())
                                            })
                                        })
                                    })
                                })
                            })?;
                        }
                        Ok(())
                    })
                }
                Ok(file_chooser::Outcome::Cancelled) => {
                    writer.array("{sv}", |_| Ok(()))
                }
                Ok(file_chooser::Outcome::Pending) | Err(_) => {
                    writer.array("{sv}", |_| Ok(()))
                }
            }
        })
        .map_err(wire_error)?
        .encode()
        .map_err(message_error)
}

fn file_chooser_response_code(outcome: &Result<file_chooser::Outcome, String>) -> u32 {
    match outcome {
        Ok(file_chooser::Outcome::Accepted(_)) => 0,
        Ok(file_chooser::Outcome::Cancelled) => 1,
        Ok(file_chooser::Outcome::Pending) | Err(_) => 2,
    }
}

fn file_chooser_completion(
    endian: Endian,
    serial: u32,
    path: &str,
    destination: &str,
    outcome: &Result<file_chooser::Outcome, String>,
    filter: Option<&file_chooser::FileFilter>,
) -> io::Result<(Vec<u8>, String)> {
    let response = file_chooser_response_code(outcome);
    let frame = file_chooser_response(endian, serial, path, destination, outcome, filter)?;
    Ok((
        frame,
        file_chooser_completion_record(path, response),
    ))
}

fn file_chooser_completion_record(path: &str, response: u32) -> String {
    format!("TD-PORTAL-FILE-CHOOSER-COMPLETED path={path} response={response}")
}

fn request_response(
    endian: Endian,
    serial: u32,
    path: &str,
    destination: &str,
    response: u32,
) -> io::Result<Vec<u8>> {
    message::Builder::signal(endian, path, handles::REQUEST_INTERFACE, "Response")
        .destination(destination)
        .serial(serial)
        .body("ua{sv}", |writer| {
            write_background_response(writer, response)
        })
        .map_err(wire_error)?
        .encode()
        .map_err(message_error)
}

fn write_background_response(writer: &mut Writer, response: u32) -> Result<(), WireError> {
    writer.uint32(response);
    writer.array("{sv}", |writer| {
        for key in ["background", "autostart"] {
            writer.dict_entry(|writer| {
                writer.string(key)?;
                writer.variant("b", |writer| {
                    writer.bool(false);
                    Ok(())
                })
            })?;
        }
        Ok(())
    })
}

fn session_closed(
    endian: Endian,
    serial: u32,
    path: &str,
    destination: &str,
) -> io::Result<Vec<u8>> {
    message::Builder::signal(endian, path, handles::SESSION_INTERFACE, "Closed")
        .destination(destination)
        .serial(serial)
        .body("a{sv}", |writer| writer.array("{sv}", |_| Ok(())))
        .map_err(wire_error)?
        .encode()
        .map_err(message_error)
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
        SETTINGS_INTERFACE
            | BACKGROUND_INTERFACE
            | FILE_CHOOSER_INTERFACE
            | PROPERTIES_INTERFACE
            | INTROSPECT_INTERFACE
            | PEER_INTERFACE
    )
}

fn interface_version(interface: &str) -> Option<u32> {
    match interface {
        SETTINGS_INTERFACE => Some(SETTINGS_VERSION),
        BACKGROUND_INTERFACE => Some(BACKGROUND_VERSION),
        FILE_CHOOSER_INTERFACE => Some(FILE_CHOOSER_VERSION),
        _ => None,
    }
}

fn unknown_interface(call: &Message<'_>, serial: u32, sender: &str) -> io::Result<Vec<u8>> {
    method_error(
        call.endian,
        serial,
        call.serial,
        sender,
        UNKNOWN_INTERFACE_ERROR,
        UNKNOWN_INTERFACE_TEXT,
    )
}

fn unknown_object(
    call: &Message<'_>,
    serial: u32,
    sender: &str,
    text: &str,
) -> io::Result<Vec<u8>> {
    method_error(
        call.endian,
        serial,
        call.serial,
        sender,
        "org.freedesktop.DBus.Error.UnknownObject",
        text,
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
    if !known_property_interface(interface) {
        return unknown_interface(call, serial, sender);
    }
    let Some(version) = interface_version(interface).filter(|_| property == "version") else {
        return unknown_property(call, serial, sender);
    };
    method_return(call.endian, serial, call.serial, sender, "v", |writer| {
        writer.variant("u", |writer| {
            writer.uint32(version);
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
        |writer| match interface_version(interface) {
            Some(version) => writer.array("{sv}", |writer| {
                writer.dict_entry(|writer| {
                    writer.string("version")?;
                    writer.variant("u", |writer| {
                        writer.uint32(version);
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
    if !known_property_interface(interface) {
        return unknown_interface(call, serial, sender);
    }
    if *property != "version" || interface_version(interface).is_none() {
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
            writer.uint32(SETTINGS_VERSION);
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
    let portal_sender = version.sender.clone();
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
    let introspection = connection
        .call(
            PORTAL_NAME,
            PORTAL_PATH,
            INTROSPECT_INTERFACE,
            "Introspect",
            "",
            |_| Ok(()),
        )
        .map_err(|error| format!("the portal introspection call failed: {error}"))?;
    if introspection.sender != portal_sender
        || introspection
            .one_string()
            .map_err(|error| error.to_string())?
            != INTROSPECTION_XML
    {
        return Err("the live portal introspection reply is not exact".into());
    }
    for (interface, member) in UNAVAILABLE_INTERFACES {
        if INTROSPECTION_XML.contains(interface) {
            return Err(format!(
                "the unavailable {interface} portal appeared in live introspection"
            ));
        }
        require_unavailable_interface(&mut connection, &portal_sender, interface, member)?;
    }
    let unique = connection
        .unique
        .as_deref()
        .ok_or_else(|| "the portal probe has no unique name".to_string())?;
    let token = "td_probe_1";
    let request_path = handles::request_path(unique, token)
        .ok_or_else(|| "the portal probe cannot derive its Request path".to_string())?;
    let match_rule = format!(
        "type='signal',interface='{}',member='Response',path='{request_path}'",
        handles::REQUEST_INTERFACE
    );
    connection
        .call(BUS_NAME, BUS_PATH, BUS_NAME, "AddMatch", "s", |writer| {
            writer.string(&match_rule)
        })
        .and_then(|reply| reply.no_body())
        .map_err(|error| format!("cannot subscribe to Request.Response: {error}"))?;
    let request = connection
        .call_guarded(
            PORTAL_NAME,
            PORTAL_PATH,
            BACKGROUND_INTERFACE,
            "RequestBackground",
            CallGuard {
                signature: "sa{sv}",
                early_response_path: Some(&request_path),
            },
            |writer| {
                writer.string("")?;
                writer.array("{sv}", |writer| {
                    writer.dict_entry(|writer| {
                        writer.string("handle_token")?;
                        writer.variant("s", |writer| writer.string(token))
                    })
                })
            },
        )
        .map_err(|error| format!("the Background request call failed: {error}"))?;
    if request
        .one_object_path()
        .map_err(|error| error.to_string())?
        != request_path
    {
        return Err("the live Background reply returned another Request path".into());
    }
    let response = connection
        .wait_request_response(&request_path, &request.sender)
        .map_err(|error| format!("the Background request response failed: {error}"))?;
    if response.endian != Endian::Little
        || response.signature != "ua{sv}"
        || response.body != BACKGROUND_DENIAL_BODY
    {
        return Err("the live Background denial response is not exact".into());
    }
    println!("{READY_MARKER}");
    println!("{REQUEST_READY_MARKER}");
    println!("{UNAVAILABLE_READY_MARKER}");
    Ok(())
}

fn require_unavailable_interface(
    connection: &mut Connection,
    portal_sender: &str,
    interface: &str,
    member: &str,
) -> Result<(), String> {
    let get = connection
        .call_outcome(
            PORTAL_NAME,
            PORTAL_PATH,
            PROPERTIES_INTERFACE,
            "Get",
            "ss",
            |writer| {
                writer.string(interface)?;
                writer.string("version")
            },
        )
        .map_err(|error| format!("the unavailable {interface} Get call failed: {error}"))?;
    require_exact_remote_error(
        get,
        portal_sender,
        interface,
        "Properties.Get",
        UNKNOWN_INTERFACE_ERROR,
        UNKNOWN_INTERFACE_TEXT,
    )?;

    let get_all = connection
        .call_outcome(
            PORTAL_NAME,
            PORTAL_PATH,
            PROPERTIES_INTERFACE,
            "GetAll",
            "s",
            |writer| writer.string(interface),
        )
        .map_err(|error| format!("the unavailable {interface} GetAll call failed: {error}"))?;
    require_exact_remote_error(
        get_all,
        portal_sender,
        interface,
        "Properties.GetAll",
        UNKNOWN_INTERFACE_ERROR,
        UNKNOWN_INTERFACE_TEXT,
    )?;

    let direct = connection
        .call_outcome(PORTAL_NAME, PORTAL_PATH, interface, member, "", |_| Ok(()))
        .map_err(|error| format!("the unavailable {interface}.{member} call failed: {error}"))?;
    require_exact_remote_error(
        direct,
        portal_sender,
        interface,
        member,
        UNKNOWN_METHOD_ERROR,
        UNKNOWN_METHOD_TEXT,
    )
}

fn require_exact_remote_error(
    outcome: CallOutcome,
    portal_sender: &str,
    interface: &str,
    operation: &str,
    name: &str,
    text: &str,
) -> Result<(), String> {
    match outcome {
        CallOutcome::Return(_) => Err(format!(
            "the unavailable {interface} portal unexpectedly answered {operation}"
        )),
        CallOutcome::Error(error) => {
            let exact = error.is_exact_string(name, text).map_err(|decode| {
                format!("the unavailable {interface} {operation} error body is malformed: {decode}")
            })?;
            if error.sender == portal_sender && exact {
                Ok(())
            } else {
                Err(format!(
                    "the unavailable {interface} portal returned the wrong {operation} result: {}",
                    error.diagnostic(operation)
                ))
            }
        }
    }
}

#[cfg(test)]
fn unavailable_get_call(connection: &mut Connection, interface: &str) -> io::Result<CallOutcome> {
    connection.call_outcome(
        PORTAL_NAME,
        PORTAL_PATH,
        PROPERTIES_INTERFACE,
        "Get",
        "ss",
        |writer| {
            writer.string(interface)?;
            writer.string("version")
        },
    )
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
    let mut handles = Handles::default();
    let session = handles
        .reserve(HandleKind::Session, ":1.1", "selftest")
        .map_err(|error| format!("selftest cannot reserve a Session: {error:?}"))?;
    let closed_bytes = session_closed(Endian::Little, 1, &session, ":1.1")
        .map_err(|error| format!("selftest cannot encode Session.Closed: {error}"))?;
    let (closed, consumed) = message::decode(&closed_bytes, 0)
        .map_err(|error| format!("selftest cannot decode Session.Closed: {error}"))?;
    if handles.lookup(&session, ":1.1") != Ok(HandleKind::Session)
        || handles.len() != 1
        || consumed != closed_bytes.len()
        || closed.kind != MessageType::Signal
        || closed.fields.path != Some(session.as_str())
        || closed.fields.destination != Some(":1.1")
        || !handles.retire(&session)
    {
        return Err("selftest cannot complete a Session lifecycle".into());
    }
    file_chooser::selftest()?;
    println!("TD-PORTAL-SELFTEST-OK");
    Ok(())
}

fn run_main(args: &[String]) -> Result<(), String> {
    let command = args.first().ok_or_else(usage)?;
    match command.as_str() {
        "supervise" => supervise(&parse_paths(args.get(1..).ok_or_else(usage)?, false)?),
        "run" => run(&parse_paths(args.get(1..).ok_or_else(usage)?, true)?),
        "probe" => probe(&parse_paths(args.get(1..).ok_or_else(usage)?, false)?),
        "channel-probe" => {
            let path = parse_channel_path(args.get(1..).ok_or_else(usage)?)?;
            wayland_channel::probe(&path)?;
            println!("{}", wayland_channel::ready_marker());
            Ok(())
        }
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
mod confinement {
    #![allow(clippy::unwrap_used)]

    const SOURCES: &[(&str, &str)] = &[
        ("file_chooser.rs", include_str!("file_chooser.rs")),
        ("handles.rs", include_str!("handles.rs")),
        ("main.rs", include_str!("main.rs")),
        ("settings.rs", include_str!("settings.rs")),
        ("sys.rs", include_str!("sys.rs")),
        ("wayland_channel.rs", include_str!("wayland_channel.rs")),
        ("wayland_dialog.rs", include_str!("wayland_dialog.rs")),
    ];
    const SYS: &str = include_str!("sys.rs");
    const DIALOG: &str = include_str!("wayland_dialog.rs");

    fn production(source: &str) -> &str {
        source.split_once("#[cfg(test)]").unwrap().0
    }

    #[test]
    fn unsafe_is_one_scoped_syscall_instruction() {
        let keyword = format!("un{}", "safe");
        let lint = format!("{keyword}_code");
        assert_eq!(
            SOURCES[2].1.matches(&format!("#![deny({lint})]")).count(),
            1
        );
        for (name, source) in SOURCES {
            let bare = production(source)
                .matches(&keyword)
                .count()
                .saturating_sub(production(source).matches(&lint).count());
            if *name == "sys.rs" {
                assert_eq!(bare, 1, "{name}");
                assert_eq!(source.matches(&format!("#[allow({lint})]")).count(), 1);
            } else {
                assert_eq!(bare, 0, "{name}");
            }
        }
        assert_eq!(SYS.matches("core::arch::asm!").count(), 1);
        assert_eq!(SYS.matches("\"syscall\"").count(), 1);
    }

    #[test]
    fn syscall_and_descriptor_rosters_are_exact() {
        for pin in [
            "const SYS_CLOSE: usize = 3;",
            "const SYS_SENDMSG: usize = 46;",
            "const SYS_RECVMSG: usize = 47;",
            "const SCM_RIGHTS: i32 = 1;",
            "const MSG_CMSG_CLOEXEC: i32 = 0x4000_0000;",
            "const MSG_NOSIGNAL: i32 = 0x4000;",
        ] {
            assert!(SYS.contains(pin), "missing syscall pin {pin}");
        }
        assert_eq!(SYS.matches("const SYS_").count(), 3);
        assert_eq!(production(SYS).matches("syscall5(SYS_CLOSE,").count(), 1);
        assert_eq!(production(SYS).matches("SYS_SENDMSG,").count(), 1);
        assert_eq!(production(SYS).matches("SYS_RECVMSG,").count(), 1);
        assert_eq!(production(SYS).matches("/proc/self/fd/{fd}").count(), 1);
        assert_eq!(production(DIALOG).matches("sys::send_with_fd(").count(), 1);
        assert_eq!(production(DIALOG).matches("sys::recv_with_fds(").count(), 1);
        assert_eq!(
            production(DIALOG)
                .matches("sys::duplicate_received(")
                .count(),
            1
        );
        assert_eq!(
            production(DIALOG).matches("sys::discard_received(").count(),
            4
        );
    }

    #[test]
    fn confinement_inventory_covers_every_portal_source() {
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut actual = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("rs"))
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        actual.sort();
        let mut expected = SOURCES
            .iter()
            .map(|(name, _)| (*name).to_string())
            .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(actual, expected);
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
        call_at(PORTAL_PATH, ":1.9", interface, member, signature, fill)
    }

    fn call_at<F>(
        path: &str,
        sender: &str,
        interface: &str,
        member: &str,
        signature: &str,
        fill: F,
    ) -> Vec<u8>
    where
        F: FnOnce(&mut Writer) -> Result<(), WireError>,
    {
        message::Builder::method_call(Endian::Little, path, Some(interface), member)
            .destination(PORTAL_NAME)
            .sender(sender)
            .serial(7)
            .body(signature, fill)
            .unwrap()
            .encode()
            .unwrap()
    }

    fn open_file_call<F>(fill_options: F) -> Vec<u8>
    where
        F: FnOnce(&mut Writer) -> Result<(), WireError>,
    {
        call(FILE_CHOOSER_INTERFACE, "OpenFile", "ssa{sv}", |writer| {
            writer.string("wayland:0123456789abcdef")?;
            writer.string("Choose a file")?;
            writer.array("{sv}", fill_options)
        })
    }

    fn write_file_filter(
        writer: &mut Writer,
        label: &str,
        rules: &[(u32, &str)],
    ) -> Result<(), WireError> {
        writer.structure(|writer| {
            writer.string(label)?;
            writer.array("(us)", |writer| {
                for (kind, pattern) in rules {
                    writer.structure(|writer| {
                        writer.uint32(*kind);
                        writer.string(pattern)
                    })?;
                }
                Ok(())
            })
        })
    }

    fn credentials_reply(reply_serial: u32, uid: u32, app_id: Option<&str>) -> Vec<u8> {
        message::Builder::method_return(Endian::Little, reply_serial)
            .sender(BUS_NAME)
            .destination(":1.10")
            .serial(90)
            .body("a{sv}", |writer| {
                writer.array("{sv}", |writer| {
                    writer.dict_entry(|writer| {
                        writer.string("UnixUserID")?;
                        writer.variant("u", |writer| {
                            writer.uint32(uid);
                            Ok(())
                        })
                    })?;
                    if let Some(app_id) = app_id {
                        writer.dict_entry(|writer| {
                            writer.string("td.AppId")?;
                            writer.variant("s", |writer| writer.string(app_id))
                        })?;
                    }
                    Ok(())
                })
            })
            .unwrap()
            .encode()
            .unwrap()
    }

    fn malformed_credentials_reply(reply_serial: u32) -> Vec<u8> {
        message::Builder::method_return(Endian::Little, reply_serial)
            .sender(BUS_NAME)
            .destination(":1.10")
            .serial(90)
            .body("s", |writer| writer.string("not a credential dictionary"))
            .unwrap()
            .encode()
            .unwrap()
    }

    fn pending_open(owner: &str, token: &str) -> PendingOpen {
        PendingOpen {
            endian: Endian::Little,
            call_serial: 7,
            owner: owner.into(),
            wants_reply: true,
            token: token.into(),
            title: "Choose a file".into(),
            parent_handle: String::new(),
            mode: file_chooser::Mode::OpenFile { multiple: false },
            accept_label: None,
            filter: None,
        }
    }

    fn dispatch_one(call: &Message<'_>, settings: &Settings, serial: u32) -> Vec<u8> {
        let mut handles = Handles::default();
        let mut serials = serial.saturating_sub(1);
        let outbound = dispatch(call, settings, &mut handles, &mut serials).unwrap();
        assert!(outbound.retire.is_none());
        assert_eq!(outbound.frames.len(), 1);
        outbound.frames.into_iter().next().unwrap()
    }

    #[test]
    fn the_four_commands_have_closed_flag_grammars() {
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
        assert_eq!(
            parse_channel_path(&strings(&[
                "--wayland",
                "/run/user/1000/td-portal-wayland-0"
            ])),
            Ok(PathBuf::from("/run/user/1000/td-portal-wayland-0"))
        );
        for bad in [
            strings(&["--wayland", "relative"]),
            strings(&["--wayland"]),
            strings(&["--wayland", "/run/portal", "extra"]),
            strings(&["--wayland", "/run/portal", "--wayland", "/run/other"]),
        ] {
            assert!(parse_channel_path(&bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn open_file_parser_accepts_the_supported_portal_shape() {
        let bytes = open_file_call(|writer| {
            writer.dict_entry(|writer| {
                writer.string("handle_token")?;
                writer.variant("s", |writer| writer.string("pick_7"))
            })?;
            writer.dict_entry(|writer| {
                writer.string("multiple")?;
                writer.variant("b", |writer| {
                    writer.bool(true);
                    Ok(())
                })
            })?;
            writer.dict_entry(|writer| {
                writer.string("modal")?;
                writer.variant("b", |writer| {
                    writer.bool(true);
                    Ok(())
                })
            })?;
            writer.dict_entry(|writer| {
                writer.string("future-option")?;
                writer.variant("s", |writer| writer.string("ignored"))
            })
        });
        let (call, _) = message::decode(&bytes, 0).unwrap();
        let parsed = parse_open_file(&call).unwrap();
        assert_eq!(parsed.token, "pick_7");
        assert_eq!(parsed.title, "Choose a file");
        assert_eq!(parsed.parent_handle, "0123456789abcdef");
        assert_eq!(parsed.mode, file_chooser::Mode::OpenFile { multiple: true });

        let malformed = call_at(
            PORTAL_PATH,
            ":1.9",
            FILE_CHOOSER_INTERFACE,
            "OpenFile",
            "ssa{sv}",
            |writer| {
                writer.string("wayland:not a handle")?;
                writer.string("Choose")?;
                writer.array("{sv}", |_| Ok(()))
            },
        );
        let (malformed, _) = message::decode(&malformed, 0).unwrap();
        assert_eq!(parse_open_file(&malformed).unwrap_err().name, INVALID_OPEN);

        let empty_title = call_at(
            PORTAL_PATH,
            ":1.9",
            FILE_CHOOSER_INTERFACE,
            "OpenFile",
            "ssa{sv}",
            |writer| {
                writer.string("wayland:0123456789abcdef")?;
                writer.string("")?;
                writer.array("{sv}", |_| Ok(()))
            },
        );
        let (empty_title, _) = message::decode(&empty_title, 0).unwrap();
        assert_eq!(parse_open_file(&empty_title).unwrap().title, "");

        let maximum_title = "a".repeat(MAX_FILE_CHOOSER_TITLE_BYTES);
        let maximum = call_at(
            PORTAL_PATH,
            ":1.9",
            FILE_CHOOSER_INTERFACE,
            "OpenFile",
            "ssa{sv}",
            |writer| {
                writer.string("wayland:0123456789abcdef")?;
                writer.string(&maximum_title)?;
                writer.array("{sv}", |_| Ok(()))
            },
        );
        let (maximum, _) = message::decode(&maximum, 0).unwrap();
        assert_eq!(
            parse_open_file(&maximum).unwrap().title.len(),
            MAX_FILE_CHOOSER_TITLE_BYTES
        );
    }

    #[test]
    fn accept_label_and_firefox_filter_list_select_all_files() {
        let accept_label = open_file_call(|writer| {
            writer.dict_entry(|writer| {
                writer.string("accept_label")?;
                writer.variant("s", |writer| writer.string("Select"))
            })
        });
        let (accept_label, _) = message::decode(&accept_label, 0).unwrap();
        assert_eq!(
            parse_open_file(&accept_label)
                .unwrap()
                .accept_label
                .as_deref(),
            Some("Select")
        );

        let firefox = open_file_call(|writer| {
            writer.dict_entry(|writer| {
                writer.string("filters")?;
                writer.variant("a(sa(us))", |writer| {
                    writer.array("(sa(us))", |writer| {
                        for (label, rules) in [
                            ("All Files", &[(0, "*")][..]),
                            ("HTML Files", &[(0, "*.html"), (1, "text/html")][..]),
                            ("Text Files", &[(1, "text/plain")][..]),
                            ("Image Files", &[(1, "image/png"), (1, "image/jpeg")][..]),
                            ("XML Files", &[(0, "*.xml")][..]),
                            ("PDF Files", &[(1, "application/pdf")][..]),
                        ] {
                            write_file_filter(writer, label, rules)?;
                        }
                        Ok(())
                    })
                })
            })?;
            writer.dict_entry(|writer| {
                writer.string("current_filter")?;
                writer.variant("(sa(us))", |writer| {
                    write_file_filter(writer, "All Files", &[(0, "*")])
                })
            })
        });
        let (firefox, _) = message::decode(&firefox, 0).unwrap();
        let parsed = parse_open_file(&firefox).unwrap();
        let filter = parsed.filter.unwrap();
        assert_eq!(filter.label(), "All Files");
        assert_eq!(filter.pattern(), "*");

        let standalone_current = open_file_call(|writer| {
            writer.dict_entry(|writer| {
                writer.string("current_filter")?;
                writer.variant("(sa(us))", |writer| {
                    writer.structure(|writer| {
                        writer.string("All Files")?;
                        writer.array("(us)", |writer| {
                            writer.structure(|writer| {
                                writer.uint32(0);
                                writer.string("*")
                            })
                        })
                    })
                })
            })
        });
        let (standalone_current, _) =
            message::decode(&standalone_current, 0).unwrap();
        assert_eq!(
            parse_open_file(&standalone_current).unwrap_err().name,
            UNSUPPORTED_OPEN
        );
    }

    #[test]
    fn a_non_all_files_current_filter_remains_explicitly_unsupported() {
        let selected_text = open_file_call(|writer| {
            writer.dict_entry(|writer| {
                writer.string("filters")?;
                writer.variant("a(sa(us))", |writer| {
                    writer.array("(sa(us))", |writer| {
                        write_file_filter(writer, "All Files", &[(0, "*")])?;
                        write_file_filter(writer, "Text", &[(0, "*.txt")])
                    })
                })
            })?;
            writer.dict_entry(|writer| {
                writer.string("current_filter")?;
                writer.variant("(sa(us))", |writer| {
                    write_file_filter(writer, "Text", &[(0, "*.txt")])
                })
            })
        });
        let (selected_text, _) = message::decode(&selected_text, 0).unwrap();
        assert_eq!(
            parse_open_file(&selected_text).unwrap_err().name,
            UNSUPPORTED_OPEN
        );

        let multiple_without_current = open_file_call(|writer| {
            writer.dict_entry(|writer| {
                writer.string("filters")?;
                writer.variant("a(sa(us))", |writer| {
                    writer.array("(sa(us))", |writer| {
                        write_file_filter(writer, "All Files", &[(0, "*")])?;
                        write_file_filter(writer, "Text", &[(0, "*.txt")])
                    })
                })
            })
        });
        let (multiple_without_current, _) =
            message::decode(&multiple_without_current, 0).unwrap();
        assert_eq!(
            parse_open_file(&multiple_without_current)
                .unwrap_err()
                .name,
            UNSUPPORTED_OPEN
        );

        let oversized = "x".repeat(257);
        let invalid_after_mime = open_file_call(|writer| {
            writer.dict_entry(|writer| {
                writer.string("filters")?;
                writer.variant("a(sa(us))", |writer| {
                    writer.array("(sa(us))", |writer| {
                        writer.structure(|writer| {
                            writer.string("Mixed")?;
                            writer.array("(us)", |writer| {
                                writer.structure(|writer| {
                                    writer.uint32(1);
                                    writer.string("text/plain")
                                })?;
                                writer.structure(|writer| {
                                    writer.uint32(2);
                                    writer.string(&oversized)
                                })
                            })
                        })
                    })
                })
            })
        });
        let (invalid_after_mime, _) =
            message::decode(&invalid_after_mime, 0).unwrap();
        assert_eq!(
            parse_open_file(&invalid_after_mime).unwrap_err().name,
            INVALID_OPEN
        );
    }

    #[test]
    fn file_filter_and_rule_counts_have_exact_closed_bounds() {
        let filters_at_limit = open_file_call(|writer| {
            writer.dict_entry(|writer| {
                writer.string("filters")?;
                writer.variant("a(sa(us))", |writer| {
                    writer.array("(sa(us))", |writer| {
                        for index in 0..32 {
                            write_file_filter(
                                writer,
                                &format!("All Files {index}"),
                                &[(0, "*")],
                            )?;
                        }
                        Ok(())
                    })
                })
            })?;
            writer.dict_entry(|writer| {
                writer.string("current_filter")?;
                writer.variant("(sa(us))", |writer| {
                    write_file_filter(writer, "All Files 31", &[(0, "*")])
                })
            })
        });
        let (filters_at_limit, _) =
            message::decode(&filters_at_limit, 0).unwrap();
        assert_eq!(
            parse_open_file(&filters_at_limit)
                .unwrap()
                .filter
                .unwrap()
                .label(),
            "All Files 31"
        );

        let filters_over_limit = open_file_call(|writer| {
            writer.dict_entry(|writer| {
                writer.string("filters")?;
                writer.variant("a(sa(us))", |writer| {
                    writer.array("(sa(us))", |writer| {
                        for index in 0..33 {
                            write_file_filter(
                                writer,
                                &format!("All Files {index}"),
                                &[(0, "*")],
                            )?;
                        }
                        Ok(())
                    })
                })
            })
        });
        let (filters_over_limit, _) =
            message::decode(&filters_over_limit, 0).unwrap();
        let error = parse_open_file(&filters_over_limit).unwrap_err();
        assert_eq!(error.name, INVALID_OPEN);
        assert_eq!(error.text, "filters exceeds 32 entries");

        let rules_at_limit = open_file_call(|writer| {
            writer.dict_entry(|writer| {
                writer.string("filters")?;
                writer.variant("a(sa(us))", |writer| {
                    writer.array("(sa(us))", |writer| {
                        let rules = vec![(0, "*"); 32];
                        write_file_filter(writer, "Many Rules", &rules)
                    })
                })
            })
        });
        let (rules_at_limit, _) =
            message::decode(&rules_at_limit, 0).unwrap();
        assert_eq!(
            parse_open_file(&rules_at_limit).unwrap_err().name,
            UNSUPPORTED_OPEN
        );

        let rules_over_limit = open_file_call(|writer| {
            writer.dict_entry(|writer| {
                writer.string("filters")?;
                writer.variant("a(sa(us))", |writer| {
                    writer.array("(sa(us))", |writer| {
                        let rules = vec![(0, "*"); 33];
                        write_file_filter(writer, "Too Many Rules", &rules)
                    })
                })
            })
        });
        let (rules_over_limit, _) =
            message::decode(&rules_over_limit, 0).unwrap();
        let error = parse_open_file(&rules_over_limit).unwrap_err();
        assert_eq!(error.name, INVALID_OPEN);
        assert_eq!(error.text, "a file filter exceeds 32 rules");
    }

    #[test]
    fn save_methods_are_advertised_but_explicitly_unsupported() {
        for method in ["OpenFile", "SaveFile", "SaveFiles"] {
            let needle = format!("<method name=\"{method}\">");
            assert_eq!(INTROSPECTION_XML.matches(&needle).count(), 1);
        }
        let settings = Settings::parse(settings::DEFAULT_CONFIG).unwrap();
        for method in ["SaveFile", "SaveFiles"] {
            let bytes = call(FILE_CHOOSER_INTERFACE, method, "ssa{sv}", |writer| {
                writer.string("")?;
                writer.string("Save")?;
                writer.array("{sv}", |_| Ok(()))
            });
            let (request, _) = message::decode(&bytes, 0).unwrap();
            let reply = dispatch_one(&request, &settings, 12);
            let (reply, _) = message::decode(&reply, 0).unwrap();
            assert_eq!(reply.fields.error_name, Some(UNSUPPORTED_OPEN));

            let malformed = call(FILE_CHOOSER_INTERFACE, method, "s", |writer| {
                writer.string("bad")
            });
            let (malformed, _) = message::decode(&malformed, 0).unwrap();
            let reply = dispatch_one(&malformed, &settings, 13);
            let (reply, _) = message::decode(&reply, 0).unwrap();
            assert_eq!(reply.fields.error_name, Some(INVALID_OPEN));
        }
    }

    #[test]
    fn open_file_first_queries_the_broker_for_its_sender_identity() {
        let bytes = open_file_call(|writer| {
            writer.dict_entry(|writer| {
                writer.string("handle_token")?;
                writer.variant("s", |writer| writer.string("brokered"))
            })
        });
        let (call, _) = message::decode(&bytes, 0).unwrap();
        let (service_stream, mut broker_stream) = UnixStream::pair().unwrap();
        broker_stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut connection = Connection {
            stream: service_stream,
            serial: 40,
            unique: Some(":1.10".into()),
            until: None,
        };
        let mut state = ServiceState::default();
        begin_open_file(&mut connection, &mut state, &call).unwrap();
        let IncomingFrame::Message(query) = read_frame(&mut broker_stream).unwrap() else {
            panic!("identity query was not a complete frame");
        };
        let (query, _) = message::decode(&query, 0).unwrap();
        assert_eq!(query.fields.destination, Some(BUS_NAME));
        assert_eq!(query.fields.member, Some("GetConnectionCredentials"));
        assert_eq!(query.args(), vec![Value::Str(":1.9")]);
        assert_eq!(state.pending.len(), 1);
        assert_eq!(connection.serial, 41);
    }

    #[test]
    fn open_file_identity_queries_have_global_and_per_owner_ceilings() {
        assert_eq!(MAX_QUEUED_SERVICE_EVENTS, 32);
        assert_eq!(MAX_PENDING_OPENS + 1, MAX_PENDING_IDENTITIES);
        assert_eq!(
            MAX_PENDING_OPENS_PER_OWNER + 1,
            MAX_PENDING_IDENTITIES_PER_OWNER
        );
        let bytes = open_file_call(|writer| {
            writer.dict_entry(|writer| {
                writer.string("handle_token")?;
                writer.variant("s", |writer| writer.string("bounded"))
            })
        });
        let (call, _) = message::decode(&bytes, 0).unwrap();
        for global in [false, true] {
            let (service_stream, mut broker_stream) = UnixStream::pair().unwrap();
            broker_stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut connection = Connection {
                stream: service_stream,
                serial: 40,
                unique: Some(":1.10".into()),
                until: None,
            };
            let mut state = ServiceState::default();
            let count = if global {
                MAX_PENDING_OPENS
            } else {
                MAX_PENDING_OPENS_PER_OWNER
            };
            for index in 0..count {
                let owner = if global {
                    format!(":1.{}", index + 100)
                } else {
                    ":1.9".into()
                };
                state
                    .pending
                    .insert(index as u32 + 1, pending_open(&owner, "existing"));
            }
            begin_open_file(&mut connection, &mut state, &call).unwrap();
            let IncomingFrame::Message(error) = read_frame(&mut broker_stream).unwrap() else {
                panic!("identity ceiling refusal was not a complete frame");
            };
            let (error, _) = message::decode(&error, 0).unwrap();
            assert_eq!(
                error.fields.error_name,
                Some("org.freedesktop.DBus.Error.LimitsExceeded")
            );
            assert_eq!(state.pending.len(), count);
            assert_eq!(connection.serial, 41);
        }
    }

    #[test]
    fn active_audits_share_open_file_identity_ceilings() {
        let bytes = open_file_call(|_| Ok(()));
        let (call, _) = message::decode(&bytes, 0).unwrap();
        for global in [false, true] {
            let (service_stream, mut broker_stream) = UnixStream::pair().unwrap();
            broker_stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut connection = Connection {
                stream: service_stream,
                serial: 40,
                unique: Some(":1.10".into()),
                until: None,
            };
            let mut state = ServiceState::default();
            let path = "/org/freedesktop/portal/desktop/request/1_8/audit";
            let audit_owner = if global { ":1.8" } else { ":1.9" };
            state.active.insert(
                path.into(),
                ActiveDialog {
                    owner: audit_owner.into(),
                    endian: Endian::Little,
                    cancel: None,
                    presented: true,
                    cancelled: false,
                    filter: None,
                },
            );
            state.pending_audits.insert(1, path.into());
            let count = if global {
                MAX_PENDING_IDENTITIES - 1
            } else {
                MAX_PENDING_IDENTITIES_PER_OWNER - 1
            };
            for index in 0..count {
                let owner = if global {
                    format!(":1.{}", index + 100)
                } else {
                    ":1.9".into()
                };
                state
                    .pending
                    .insert(index as u32 + 2, pending_open(&owner, "existing"));
            }
            begin_open_file(&mut connection, &mut state, &call).unwrap();
            let IncomingFrame::Message(error) = read_frame(&mut broker_stream).unwrap() else {
                panic!("shared identity ceiling refusal was not a complete frame");
            };
            let (error, _) = message::decode(&error, 0).unwrap();
            assert_eq!(
                error.fields.error_name,
                Some("org.freedesktop.DBus.Error.LimitsExceeded")
            );
            assert_eq!(pending_identity_count(&state), count + 1);
        }
    }

    #[test]
    fn owner_audits_keep_a_reserved_lane_through_saturation() {
        for global in [false, true] {
            let (service_stream, mut broker_stream) = UnixStream::pair().unwrap();
            broker_stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut connection = Connection {
                stream: service_stream,
                serial: 40,
                unique: Some(":1.10".into()),
                until: None,
            };
            let mut state = ServiceState::default();
            let path = "/org/freedesktop/portal/desktop/request/1_9/audit";
            state.active.insert(
                path.into(),
                ActiveDialog {
                    owner: ":1.9".into(),
                    endian: Endian::Little,
                    cancel: None,
                    presented: true,
                    cancelled: false,
                    filter: None,
                },
            );
            let count = if global {
                MAX_PENDING_OPENS
            } else {
                MAX_PENDING_OPENS_PER_OWNER
            };
            for index in 0..count {
                let owner = if global {
                    format!(":1.{}", index + 100)
                } else {
                    ":1.9".into()
                };
                state
                    .pending
                    .insert(index as u32 + 1, pending_open(&owner, "existing"));
            }
            begin_active_audit(&mut connection, &mut state).unwrap();
            let IncomingFrame::Message(first_query) = read_frame(&mut broker_stream).unwrap()
            else {
                panic!("reserved owner audit was not a complete frame");
            };
            let (first_query, _) = message::decode(&first_query, 0).unwrap();
            assert_eq!(first_query.fields.member, Some("GetConnectionCredentials"));
            assert_eq!(
                state.pending_audits.get(&41).map(String::as_str),
                Some(path)
            );
            assert_eq!(pending_identity_count(&state), count + 1);
            assert!(pending_identity_count(&state) <= MAX_PENDING_IDENTITIES);
            assert!(
                pending_identity_count_for_owner(&state, ":1.9")
                    <= MAX_PENDING_IDENTITIES_PER_OWNER
            );

            let live = credentials_reply(41, UI_UID, Some("firefox"));
            let (live, _) = message::decode(&live, 0).unwrap();
            assert!(consume_active_audit_reply(&mut connection, &mut state, &live).unwrap());
            assert!(state.pending_audits.is_empty());

            let bytes = open_file_call(|_| Ok(()));
            let (call, _) = message::decode(&bytes, 0).unwrap();
            begin_open_file(&mut connection, &mut state, &call).unwrap();
            let IncomingFrame::Message(refusal) = read_frame(&mut broker_stream).unwrap() else {
                panic!("saturated OpenFile refusal was not a complete frame");
            };
            let (refusal, _) = message::decode(&refusal, 0).unwrap();
            assert_eq!(
                refusal.fields.error_name,
                Some("org.freedesktop.DBus.Error.LimitsExceeded")
            );

            begin_active_audit(&mut connection, &mut state).unwrap();
            let IncomingFrame::Message(second_query) = read_frame(&mut broker_stream).unwrap()
            else {
                panic!("second reserved owner audit was not a complete frame");
            };
            let (second_query, _) = message::decode(&second_query, 0).unwrap();
            assert_eq!(second_query.fields.member, Some("GetConnectionCredentials"));
            assert_eq!(
                state.pending_audits.get(&43).map(String::as_str),
                Some(path)
            );
        }
    }

    #[test]
    fn invalid_app_identity_cannot_reach_the_download_grant_or_stop_the_service() {
        for (uid, app_id, malformed) in [
            (UI_UID, None, false),
            (UI_UID, Some("not-firefox"), false),
            (0, Some("firefox"), false),
            (UI_UID, Some("firefox"), true),
        ] {
            let (service_stream, mut caller_stream) = UnixStream::pair().unwrap();
            caller_stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut connection = Connection {
                stream: service_stream,
                serial: 50,
                unique: Some(":1.10".into()),
                until: None,
            };
            let mut state = ServiceState::default();
            state.pending.insert(41, pending_open(":1.9", "denied"));
            let reply = if malformed {
                malformed_credentials_reply(41)
            } else {
                credentials_reply(41, uid, app_id)
            };
            let (reply, _) = message::decode(&reply, 0).unwrap();
            let (events, _) = mpsc::sync_channel(MAX_QUEUED_SERVICE_EVENTS);
            assert!(consume_identity_reply(&mut connection, &mut state, &events, &reply).unwrap());
            let IncomingFrame::Message(error) = read_frame(&mut caller_stream).unwrap() else {
                panic!("identity refusal was not a complete frame");
            };
            let (error, _) = message::decode(&error, 0).unwrap();
            assert_eq!(
                error.fields.error_name,
                Some("org.freedesktop.portal.Error.NotAllowed")
            );
            assert!(state.pending.is_empty());
            assert!(state.active.is_empty());
            assert_eq!(state.handles.len(), 0);
        }
    }

    #[test]
    fn cancelled_connecting_worker_refuses_an_authenticated_second_dialog() {
        let (service_stream, mut caller_stream) = UnixStream::pair().unwrap();
        caller_stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut connection = Connection {
            stream: service_stream,
            serial: 50,
            unique: Some(":1.10".into()),
            until: None,
        };
        let mut state = ServiceState::default();
        state.pending.insert(41, pending_open(":1.9", "second"));
        let first = "/org/freedesktop/portal/desktop/request/1_8/first";
        state.active.insert(
            first.into(),
            ActiveDialog {
                owner: ":1.8".into(),
                endian: Endian::Little,
                cancel: None,
                presented: true,
                cancelled: false,
                filter: None,
            },
        );
        cancel_dialog(&mut state, first).unwrap();
        assert!(state.active.get(first).unwrap().cancelled);
        let reply = credentials_reply(41, UI_UID, Some("firefox"));
        let (reply, _) = message::decode(&reply, 0).unwrap();
        let (events, _) = mpsc::sync_channel(MAX_QUEUED_SERVICE_EVENTS);
        assert!(consume_identity_reply(&mut connection, &mut state, &events, &reply).unwrap());
        let IncomingFrame::Message(error) = read_frame(&mut caller_stream).unwrap() else {
            panic!("active-dialog refusal was not a complete frame");
        };
        let (error, _) = message::decode(&error, 0).unwrap();
        assert_eq!(
            error.fields.error_name,
            Some("org.freedesktop.DBus.Error.LimitsExceeded")
        );
        assert!(state.pending.is_empty());
        assert_eq!(state.active.len(), MAX_ACTIVE_FILE_CHOOSERS);
        assert_eq!(state.handles.len(), 0);
    }

    #[test]
    fn cancelling_a_dialog_closes_its_worker_connection() {
        let path = "/org/freedesktop/portal/desktop/request/1_9/cancel";
        let (service_stream, mut worker_stream) = UnixStream::pair().unwrap();
        worker_stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut state = ServiceState::default();
        state.active.insert(
            path.into(),
            ActiveDialog {
                owner: ":1.9".into(),
                endian: Endian::Little,
                cancel: Some(service_stream),
                presented: true,
                cancelled: false,
                filter: None,
            },
        );
        cancel_dialog(&mut state, path).unwrap();
        assert_eq!(state.active.len(), 1);
        assert!(state.active.get(path).unwrap().cancelled);
        let mut byte = [0_u8; 1];
        assert_eq!(worker_stream.read(&mut byte).unwrap(), 0);
    }

    #[test]
    fn active_owner_audit_retires_and_cancels_a_departed_caller() {
        let (service_stream, mut broker_stream) = UnixStream::pair().unwrap();
        broker_stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut connection = Connection {
            stream: service_stream,
            serial: 50,
            unique: Some(":1.10".into()),
            until: None,
        };
        let mut state = ServiceState::default();
        let path = state
            .handles
            .reserve(HandleKind::Request, ":1.9", "audited")
            .unwrap();
        let (cancel_stream, mut worker_stream) = UnixStream::pair().unwrap();
        worker_stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        state.active.insert(
            path.clone(),
            ActiveDialog {
                owner: ":1.9".into(),
                endian: Endian::Little,
                cancel: Some(cancel_stream),
                presented: true,
                cancelled: false,
                filter: None,
            },
        );
        begin_active_audit(&mut connection, &mut state).unwrap();
        let IncomingFrame::Message(query) = read_frame(&mut broker_stream).unwrap() else {
            panic!("owner audit was not a complete frame");
        };
        let (query, _) = message::decode(&query, 0).unwrap();
        assert_eq!(query.fields.member, Some("GetConnectionCredentials"));
        assert_eq!(query.args(), vec![Value::Str(":1.9")]);
        assert_eq!(state.pending_audits.get(&51), Some(&path));

        let error = message::Builder::error(
            Endian::Little,
            "org.freedesktop.DBus.Error.NameHasNoOwner",
            51,
        )
        .sender(BUS_NAME)
        .destination(":1.10")
        .serial(90)
        .encode()
        .unwrap();
        let (error, _) = message::decode(&error, 0).unwrap();
        assert!(consume_active_audit_reply(&mut connection, &mut state, &error).unwrap());
        let IncomingFrame::Message(response) = read_frame(&mut broker_stream).unwrap() else {
            panic!("lost-owner response was not a complete frame");
        };
        let (response, _) = message::decode(&response, 0).unwrap();
        assert_eq!(response.fields.path, Some(path.as_str()));
        assert_eq!(response.fields.destination, Some(":1.9"));
        assert_eq!(response.args().first(), Some(&Value::Uint32(2)));
        assert!(state.pending_audits.is_empty());
        assert_eq!(state.handles.len(), 0);
        assert!(state.active.get(&path).unwrap().cancelled);
        let mut byte = [0_u8; 1];
        assert_eq!(worker_stream.read(&mut byte).unwrap(), 0);
    }

    #[test]
    fn accepted_file_chooser_response_is_directed_and_carries_guest_uris() {
        let path = "/org/freedesktop/portal/desktop/request/1_9/pick";
        let filter = file_chooser::FileFilter::all_files("All Files", "*").unwrap();
        assert_eq!(
            file_chooser_response_code(&Ok(file_chooser::Outcome::Accepted(Vec::new()))),
            0
        );
        assert_eq!(
            file_chooser_response_code(&Ok(file_chooser::Outcome::Cancelled)),
            1
        );
        assert_eq!(
            file_chooser_completion_record(path, 1),
            format!("TD-PORTAL-FILE-CHOOSER-COMPLETED path={path} response=1")
        );
        let (cancelled, cancelled_record) = file_chooser_completion(
            Endian::Little,
            70,
            path,
            ":1.9",
            &Ok(file_chooser::Outcome::Cancelled),
            None,
        )
        .unwrap();
        let (cancelled, _) = message::decode(&cancelled, 0).unwrap();
        assert_eq!(cancelled.args().first(), Some(&Value::Uint32(1)));
        assert_eq!(
            cancelled_record,
            format!("TD-PORTAL-FILE-CHOOSER-COMPLETED path={path} response=1")
        );
        assert_eq!(
            file_chooser_response_code(&Ok(file_chooser::Outcome::Pending)),
            2
        );
        assert_eq!(file_chooser_response_code(&Err("failed".into())), 2);
        let bytes = file_chooser_response(
            Endian::Little,
            71,
            path,
            ":1.9",
            &Ok(file_chooser::Outcome::Accepted(vec![
                "file:///home/td/Downloads/report.txt".into(),
            ])),
            Some(&filter),
        )
        .unwrap();
        let (response, _) = message::decode(&bytes, 0).unwrap();
        assert_eq!(response.kind, MessageType::Signal);
        assert_eq!(response.fields.destination, Some(":1.9"));
        assert_eq!(response.fields.path, Some(path));
        let [Value::Uint32(0), Value::Array(results)] = response.args() else {
            panic!("accepted response has the wrong shape");
        };
        let entries = results.values(2).unwrap();
        let pair = entries[0].as_seq().unwrap().values(2).unwrap();
        assert_eq!(pair[0], Value::Str("uris"));
        let Value::Variant(uris) = pair[1] else {
            panic!("uris result is not a variant");
        };
        let values = uris.values(1).unwrap();
        let Value::Array(uris) = values[0] else {
            panic!("uris result is not an array");
        };
        assert_eq!(
            uris.values(1).unwrap(),
            vec![Value::Str("file:///home/td/Downloads/report.txt")]
        );
        let pair = entries[1].as_seq().unwrap().values(2).unwrap();
        assert_eq!(pair[0], Value::Str("current_filter"));
        let Value::Variant(current) = pair[1] else {
            panic!("current_filter result is not a variant");
        };
        let values = current.values(1).unwrap();
        let Value::Struct(current) = values[0] else {
            panic!("current_filter result is not a structure");
        };
        let fields = current.values(2).unwrap();
        assert_eq!(fields[0], Value::Str("All Files"));
        let Value::Array(rules) = fields[1] else {
            panic!("current_filter rules are not an array");
        };
        let rules = rules.values(1).unwrap();
        let Value::Struct(rule) = rules[0] else {
            panic!("current_filter rule is not a structure");
        };
        assert_eq!(
            rule.values(2).unwrap(),
            vec![Value::Uint32(0), Value::Str("*")]
        );
    }

    #[test]
    fn pre_presentation_completion_preserves_the_worker_diagnostic() {
        let failure = require_presented_outcome(false, Err("registry mismatch".into()));
        assert_eq!(failure.unwrap_err(), "registry mismatch");
        let success = require_presented_outcome(
            false,
            Ok(file_chooser::Outcome::Accepted(vec![
                "file:///home/td/Downloads/report.txt".into(),
            ])),
        );
        assert_eq!(
            success.unwrap_err(),
            "FileChooser completed before a presented modal frame"
        );
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
        let reply = dispatch_one(&request, &settings, 10);
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
        let reply = dispatch_one(&request, &settings, 11);
        let (reply, _) = message::decode(&reply, 0).unwrap();
        assert_eq!(reply.fields.signature, Some("v"));
        assert_eq!(reply.body_bytes(), expected_version().unwrap());
    }

    #[test]
    fn file_chooser_publishes_version_three() {
        let settings = Settings::parse(settings::DEFAULT_CONFIG).unwrap();
        let bytes = call(PROPERTIES_INTERFACE, "Get", "ss", |writer| {
            writer.string(FILE_CHOOSER_INTERFACE)?;
            writer.string("version")
        });
        let (request, _) = message::decode(&bytes, 0).unwrap();
        let reply = dispatch_one(&request, &settings, 12);
        let (reply, _) = message::decode(&reply, 0).unwrap();
        let [Value::Variant(version)] = reply.args() else {
            panic!("FileChooser version is not a variant");
        };
        assert_eq!(version.values(1).unwrap(), vec![Value::Uint32(3)]);
    }

    #[test]
    fn properties_refuse_an_ambiguous_empty_interface_and_are_read_only() {
        let settings = Settings::parse(settings::DEFAULT_CONFIG).unwrap();
        let get = call(PROPERTIES_INTERFACE, "Get", "ss", |writer| {
            writer.string("")?;
            writer.string("version")
        });
        let (request, _) = message::decode(&get, 0).unwrap();
        let reply = dispatch_one(&request, &settings, 12);
        let (reply, _) = message::decode(&reply, 0).unwrap();
        assert_eq!(
            reply.fields.error_name,
            Some("org.freedesktop.DBus.Error.UnknownInterface")
        );

        let background = call(PROPERTIES_INTERFACE, "Get", "ss", |writer| {
            writer.string(BACKGROUND_INTERFACE)?;
            writer.string("version")
        });
        let (request, _) = message::decode(&background, 0).unwrap();
        let reply = dispatch_one(&request, &settings, 13);
        let (reply, _) = message::decode(&reply, 0).unwrap();
        assert_eq!(reply.body_bytes(), expected_version().unwrap());

        let get_all = call(PROPERTIES_INTERFACE, "GetAll", "s", |writer| {
            writer.string(PEER_INTERFACE)
        });
        let (request, _) = message::decode(&get_all, 0).unwrap();
        let reply = dispatch_one(&request, &settings, 14);
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
        let reply = dispatch_one(&request, &settings, 15);
        let (reply, _) = message::decode(&reply, 0).unwrap();
        assert_eq!(
            reply.fields.error_name,
            Some("org.freedesktop.DBus.Error.PropertyReadOnly")
        );
    }

    #[test]
    fn background_exports_before_reply_then_directs_an_exact_denial() {
        let settings = Settings::parse(settings::DEFAULT_CONFIG).unwrap();
        let bytes = call(
            BACKGROUND_INTERFACE,
            "RequestBackground",
            "sa{sv}",
            |writer| {
                writer.string("")?;
                writer.array("{sv}", |writer| {
                    writer.dict_entry(|writer| {
                        writer.string("handle_token")?;
                        writer.variant("s", |writer| writer.string("probe_7"))
                    })
                })
            },
        );
        let (request, _) = message::decode(&bytes, 0).unwrap();
        let mut handles = Handles::default();
        let mut serials = 19;
        let outbound = dispatch(&request, &settings, &mut handles, &mut serials).unwrap();
        let path = "/org/freedesktop/portal/desktop/request/1_9/probe_7";
        assert_eq!(handles.lookup(path, ":1.9"), Ok(HandleKind::Request));
        assert_eq!(outbound.retire.as_deref(), Some(path));
        assert_eq!(outbound.frames.len(), 2);

        let (reply, _) = message::decode(&outbound.frames[0], 0).unwrap();
        assert_eq!(reply.kind, MessageType::MethodReturn);
        assert_eq!(reply.fields.reply_serial, Some(7));
        assert_eq!(reply.fields.destination, Some(":1.9"));
        assert_eq!(reply.args(), vec![Value::ObjectPath(path)]);

        let (response, _) = message::decode(&outbound.frames[1], 0).unwrap();
        assert_eq!(response.kind, MessageType::Signal);
        assert_eq!(response.fields.path, Some(path));
        assert_eq!(response.fields.interface, Some(handles::REQUEST_INTERFACE));
        assert_eq!(response.fields.member, Some("Response"));
        assert_eq!(response.fields.destination, Some(":1.9"));
        assert_eq!(response.fields.signature, Some("ua{sv}"));
        assert_eq!(response.body_bytes(), BACKGROUND_DENIAL_BODY);

        assert!(handles.retire(path));
        assert_eq!(handles.lookup(path, ":1.9"), Err(LookupError::Missing));
    }

    #[test]
    fn background_options_are_bounded_and_an_absent_token_is_deterministic() {
        let settings = Settings::parse(settings::DEFAULT_CONFIG).unwrap();
        let too_many = call(
            BACKGROUND_INTERFACE,
            "RequestBackground",
            "sa{sv}",
            |writer| {
                writer.string("")?;
                writer.array("{sv}", |writer| {
                    for _ in 0..=MAX_PORTAL_OPTIONS {
                        writer.dict_entry(|writer| {
                            writer.string("unknown")?;
                            writer.variant("b", |writer| {
                                writer.bool(false);
                                Ok(())
                            })?;
                            Ok(())
                        })?;
                    }
                    Ok(())
                })
            },
        );
        let (too_many, _) = message::decode(&too_many, 0).unwrap();
        let mut handles = Handles::default();
        let mut serials = 40;
        let refusal = dispatch(&too_many, &settings, &mut handles, &mut serials).unwrap();
        let (refusal, _) = message::decode(&refusal.frames[0], 0).unwrap();
        assert_eq!(
            refusal.fields.error_name,
            Some("org.freedesktop.DBus.Error.InvalidArgs")
        );
        assert_eq!(handles.len(), 0);

        let no_token = call(
            BACKGROUND_INTERFACE,
            "RequestBackground",
            "sa{sv}",
            |writer| {
                writer.string("")?;
                writer.array("{sv}", |_| Ok(()))
            },
        );
        let (no_token, _) = message::decode(&no_token, 0).unwrap();
        let outbound = dispatch(&no_token, &settings, &mut handles, &mut serials).unwrap();
        assert_eq!(
            outbound.retire.as_deref(),
            Some("/org/freedesktop/portal/desktop/request/1_9/td_7")
        );
    }

    #[test]
    fn a_no_reply_background_call_still_completes_its_known_request() {
        let settings = Settings::parse(settings::DEFAULT_CONFIG).unwrap();
        let bytes = message::Builder::method_call(
            Endian::Little,
            PORTAL_PATH,
            Some(BACKGROUND_INTERFACE),
            "RequestBackground",
        )
        .destination(PORTAL_NAME)
        .sender(":1.9")
        .flags(message::FLAG_NO_REPLY_EXPECTED)
        .serial(8)
        .body("sa{sv}", |writer| {
            writer.string("")?;
            writer.array("{sv}", |writer| {
                writer.dict_entry(|writer| {
                    writer.string("handle_token")?;
                    writer.variant("s", |writer| writer.string("silent"))
                })
            })
        })
        .unwrap()
        .encode()
        .unwrap();
        let (request, _) = message::decode(&bytes, 0).unwrap();
        let mut handles = Handles::default();
        let mut serials = 50;
        let outbound = dispatch(&request, &settings, &mut handles, &mut serials).unwrap();
        assert_eq!(outbound.frames.len(), 1);
        let (response, _) = message::decode(&outbound.frames[0], 0).unwrap();
        assert_eq!(response.kind, MessageType::Signal);
        assert_eq!(response.fields.member, Some("Response"));
        assert_eq!(
            outbound.retire.as_deref(),
            Some("/org/freedesktop/portal/desktop/request/1_9/silent")
        );
    }

    #[test]
    fn request_close_is_owner_only_and_emits_no_response() {
        let settings = Settings::parse(settings::DEFAULT_CONFIG).unwrap();
        let mut handles = Handles::default();
        let path = handles
            .reserve(HandleKind::Request, ":1.9", "pending")
            .unwrap();
        let foreign = call_at(
            &path,
            ":1.8",
            handles::REQUEST_INTERFACE,
            "Close",
            "",
            |_| Ok(()),
        );
        let (foreign, _) = message::decode(&foreign, 0).unwrap();
        let mut serials = 20;
        let refusal = dispatch(&foreign, &settings, &mut handles, &mut serials).unwrap();
        assert!(refusal.retire.is_none());
        let (refusal, _) = message::decode(&refusal.frames[0], 0).unwrap();
        assert_eq!(
            refusal.fields.error_name,
            Some("org.freedesktop.DBus.Error.UnknownObject")
        );

        let absent = call_at(
            "/org/freedesktop/portal/desktop/request/1_9/absent",
            ":1.8",
            handles::REQUEST_INTERFACE,
            "Close",
            "",
            |_| Ok(()),
        );
        let (absent, _) = message::decode(&absent, 0).unwrap();
        let absent = dispatch(&absent, &settings, &mut handles, &mut serials).unwrap();
        let (absent, _) = message::decode(&absent.frames[0], 0).unwrap();
        assert_eq!(refusal.fields.error_name, absent.fields.error_name);

        let close = call_at(
            &path,
            ":1.9",
            handles::REQUEST_INTERFACE,
            "Close",
            "",
            |_| Ok(()),
        );
        let (close, _) = message::decode(&close, 0).unwrap();
        let closed = dispatch(&close, &settings, &mut handles, &mut serials).unwrap();
        assert_eq!(closed.frames.len(), 1);
        let (reply, _) = message::decode(&closed.frames[0], 0).unwrap();
        assert_eq!(reply.kind, MessageType::MethodReturn);
        assert_eq!(closed.retire.as_deref(), Some(path.as_str()));
        assert_eq!(handles.lookup(&path, ":1.9"), Ok(HandleKind::Request));
        assert!(handles.retire(&path));
    }

    #[test]
    fn session_close_replies_without_closed_and_retires() {
        let settings = Settings::parse(settings::DEFAULT_CONFIG).unwrap();
        let mut handles = Handles::default();
        let path = handles
            .reserve(HandleKind::Session, ":1.9", "shortcuts")
            .unwrap();
        let get = call_at(&path, ":1.9", PROPERTIES_INTERFACE, "Get", "ss", |writer| {
            writer.string(handles::SESSION_INTERFACE)?;
            writer.string("version")
        });
        let (get, _) = message::decode(&get, 0).unwrap();
        let mut serials = 30;
        let property = dispatch(&get, &settings, &mut handles, &mut serials).unwrap();
        let (property, _) = message::decode(&property.frames[0], 0).unwrap();
        assert_eq!(property.body_bytes(), expected_version().unwrap());

        let introspect = call_at(
            &path,
            ":1.9",
            INTROSPECT_INTERFACE,
            "Introspect",
            "",
            |_| Ok(()),
        );
        let (introspect, _) = message::decode(&introspect, 0).unwrap();
        let answer = dispatch(&introspect, &settings, &mut handles, &mut serials).unwrap();
        let (answer, _) = message::decode(&answer.frames[0], 0).unwrap();
        assert!(answer
            .args()
            .first()
            .and_then(Value::as_str)
            .unwrap()
            .contains(handles::SESSION_INTERFACE));

        let close = call_at(
            &path,
            ":1.9",
            handles::SESSION_INTERFACE,
            "Close",
            "",
            |_| Ok(()),
        );
        let (close, _) = message::decode(&close, 0).unwrap();
        let outbound = dispatch(&close, &settings, &mut handles, &mut serials).unwrap();
        assert_eq!(outbound.frames.len(), 1);
        let (reply, _) = message::decode(&outbound.frames[0], 0).unwrap();
        assert_eq!(reply.kind, MessageType::MethodReturn);
        assert_eq!(outbound.retire.as_deref(), Some(path.as_str()));
        assert_eq!(handles.lookup(&path, ":1.9"), Ok(HandleKind::Session));
    }

    #[test]
    fn handle_properties_accept_empty_and_empty_standard_interfaces() {
        let settings = Settings::parse(settings::DEFAULT_CONFIG).unwrap();
        let mut handles = Handles::default();
        let path = handles
            .reserve(HandleKind::Session, ":1.9", "properties")
            .unwrap();
        let mut serials = 50;

        let get = call_at(&path, ":1.9", PROPERTIES_INTERFACE, "Get", "ss", |writer| {
            writer.string("")?;
            writer.string("version")
        });
        let (get, _) = message::decode(&get, 0).unwrap();
        let get = dispatch(&get, &settings, &mut handles, &mut serials).unwrap();
        let (get, _) = message::decode(&get.frames[0], 0).unwrap();
        assert_eq!(get.body_bytes(), expected_version().unwrap());

        let get_all = call_at(
            &path,
            ":1.9",
            PROPERTIES_INTERFACE,
            "GetAll",
            "s",
            |writer| writer.string(PROPERTIES_INTERFACE),
        );
        let (get_all, _) = message::decode(&get_all, 0).unwrap();
        let get_all = dispatch(&get_all, &settings, &mut handles, &mut serials).unwrap();
        let (get_all, _) = message::decode(&get_all.frames[0], 0).unwrap();
        assert_eq!(get_all.fields.signature, Some("a{sv}"));
        assert!(get_all
            .args()
            .first()
            .and_then(Value::as_seq)
            .unwrap()
            .values(1)
            .unwrap()
            .is_empty());

        let set = call_at(
            &path,
            ":1.9",
            PROPERTIES_INTERFACE,
            "Set",
            "ssv",
            |writer| {
                writer.string("")?;
                writer.string("version")?;
                writer.variant("u", |writer| {
                    writer.uint32(2);
                    Ok(())
                })
            },
        );
        let (set, _) = message::decode(&set, 0).unwrap();
        let set = dispatch(&set, &settings, &mut handles, &mut serials).unwrap();
        let (set, _) = message::decode(&set.frames[0], 0).unwrap();
        assert_eq!(
            set.fields.error_name,
            Some("org.freedesktop.DBus.Error.PropertyReadOnly")
        );
    }

    #[test]
    fn portal_initiated_session_closed_is_directed() {
        let path = "/org/freedesktop/portal/desktop/session/1_9/shortcuts";
        let bytes = session_closed(Endian::Little, 31, path, ":1.9").unwrap();
        let (closed, _) = message::decode(&bytes, 0).unwrap();
        assert_eq!(closed.kind, MessageType::Signal);
        assert_eq!(closed.fields.path, Some(path));
        assert_eq!(closed.fields.interface, Some(handles::SESSION_INTERFACE));
        assert_eq!(closed.fields.member, Some("Closed"));
        assert_eq!(closed.fields.destination, Some(":1.9"));
        assert_eq!(closed.fields.signature, Some("a{sv}"));
    }

    #[test]
    fn peer_ping_is_path_independent_without_revealing_handles() {
        let settings = Settings::parse(settings::DEFAULT_CONFIG).unwrap();
        let ping = call_at(
            "/org/example/Thing",
            ":1.8",
            PEER_INTERFACE,
            "Ping",
            "",
            |_| Ok(()),
        );
        let (ping, _) = message::decode(&ping, 0).unwrap();
        let reply = dispatch_one(&ping, &settings, 40);
        let (reply, _) = message::decode(&reply, 0).unwrap();
        assert_eq!(reply.kind, MessageType::MethodReturn);
        assert!(reply.body_bytes().is_empty());
    }

    #[test]
    fn owner_departure_removes_every_handle_without_output() {
        let settings = Settings::parse(settings::DEFAULT_CONFIG).unwrap();
        let mut handles = Handles::default();
        handles.reserve(HandleKind::Request, ":1.9", "one").unwrap();
        handles.reserve(HandleKind::Session, ":1.9", "two").unwrap();
        let signal =
            message::Builder::signal(Endian::Little, BUS_PATH, BUS_NAME, "NameOwnerChanged")
                .sender(BUS_NAME)
                .serial(50)
                .body("sss", |writer| {
                    writer.string(":1.9")?;
                    writer.string(":1.9")?;
                    writer.string("")
                })
                .unwrap()
                .encode()
                .unwrap();
        let (signal, _) = message::decode(&signal, 0).unwrap();
        let mut serials = 60;
        let outbound = dispatch(&signal, &settings, &mut handles, &mut serials).unwrap();
        assert!(outbound.frames.is_empty());
        assert_eq!(handles.len(), 0);
        assert_eq!(serials, 60);
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
                "TD-PORTAL-READY namespaces={NAMESPACE_COUNT} settings={} version={SETTINGS_VERSION}",
                settings::SETTING_COUNT
            )
        );
        assert_eq!(SETTINGS_VERSION, 1);
    }

    #[test]
    fn request_ready_marker_is_the_denied_background_contract() {
        assert_eq!(REQUEST_READY_MARKER, "TD-PORTAL-REQUEST-READY response=2");
    }

    #[test]
    fn unsupported_portals_are_absent_and_return_unknown_interface() {
        let settings = Settings::parse(settings::DEFAULT_CONFIG).unwrap();
        assert_eq!(
            UNKNOWN_INTERFACE_ERROR,
            "org.freedesktop.DBus.Error.UnknownInterface"
        );
        assert_eq!(
            UNKNOWN_INTERFACE_TEXT,
            "that portal interface is not published"
        );
        assert_eq!(
            UNKNOWN_METHOD_ERROR,
            "org.freedesktop.DBus.Error.UnknownMethod"
        );
        assert_eq!(UNKNOWN_METHOD_TEXT, "td-portal does not serve that method");
        assert_eq!(
            UNAVAILABLE_INTERFACES,
            [
                ("org.freedesktop.portal.ScreenCast", "CreateSession"),
                ("org.freedesktop.portal.RemoteDesktop", "CreateSession"),
                ("org.freedesktop.portal.Camera", "AccessCamera"),
                ("org.freedesktop.portal.Secret", "RetrieveSecret"),
                ("org.freedesktop.portal.Print", "Print"),
            ]
        );
        for (offset, (interface, member)) in UNAVAILABLE_INTERFACES.iter().enumerate() {
            assert!(!INTROSPECTION_XML.contains(interface));
            let bytes = call(PROPERTIES_INTERFACE, "Get", "ss", |writer| {
                writer.string(interface)?;
                writer.string("version")
            });
            let (request, _) = message::decode(&bytes, 0).unwrap();
            let reply = dispatch_one(&request, &settings, 20 + offset as u32);
            let (reply, _) = message::decode(&reply, 0).unwrap();
            assert_eq!(reply.kind, MessageType::Error);
            assert_eq!(reply.fields.error_name, Some(UNKNOWN_INTERFACE_ERROR));
            assert_eq!(reply.fields.signature, Some("s"));
            assert_eq!(reply.args(), vec![Value::Str(UNKNOWN_INTERFACE_TEXT)]);

            let bytes = call(PROPERTIES_INTERFACE, "GetAll", "s", |writer| {
                writer.string(interface)
            });
            let (request, _) = message::decode(&bytes, 0).unwrap();
            let reply = dispatch_one(&request, &settings, 40 + offset as u32);
            let (reply, _) = message::decode(&reply, 0).unwrap();
            assert_eq!(reply.kind, MessageType::Error);
            assert_eq!(reply.fields.error_name, Some(UNKNOWN_INTERFACE_ERROR));
            assert_eq!(reply.fields.signature, Some("s"));
            assert_eq!(reply.args(), vec![Value::Str(UNKNOWN_INTERFACE_TEXT)]);

            let bytes = call(interface, member, "", |_| Ok(()));
            let (request, _) = message::decode(&bytes, 0).unwrap();
            let reply = dispatch_one(&request, &settings, 60 + offset as u32);
            let (reply, _) = message::decode(&reply, 0).unwrap();
            assert_eq!(reply.kind, MessageType::Error);
            assert_eq!(reply.fields.error_name, Some(UNKNOWN_METHOD_ERROR));
            assert_eq!(reply.fields.signature, Some("s"));
            assert_eq!(reply.args(), vec![Value::Str(UNKNOWN_METHOD_TEXT)]);
        }
        assert_eq!(
            UNAVAILABLE_READY_MARKER,
            "TD-PORTAL-UNAVAILABLE-READY interfaces=5 error=UnknownInterface"
        );
    }

    fn scripted_remote_error(
        sender: &str,
        name: &str,
        signature: &str,
        fill: impl FnOnce(&mut Writer) -> Result<(), WireError>,
    ) -> (Connection, UnixStream) {
        let bytes = message::Builder::error(Endian::Little, name, 1)
            .sender(sender)
            .destination(":1.9")
            .serial(30)
            .body(signature, fill)
            .unwrap()
            .encode()
            .unwrap();
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        server_stream.write_all(&bytes).unwrap();
        (
            Connection {
                stream: client_stream,
                serial: 0,
                unique: Some(":1.9".into()),
                until: Some(Instant::now() + Duration::from_secs(2)),
            },
            server_stream,
        )
    }

    #[test]
    fn unavailable_probe_requires_the_complete_exact_error_body() {
        let interface = UNAVAILABLE_INTERFACES[0].0;
        let (mut exact, _server) =
            scripted_remote_error(":1.10", UNKNOWN_INTERFACE_ERROR, "s", |writer| {
                writer.string(UNKNOWN_INTERFACE_TEXT)
            });
        let outcome = unavailable_get_call(&mut exact, interface).unwrap();
        assert!(require_exact_remote_error(
            outcome,
            ":1.10",
            interface,
            "Properties.Get",
            UNKNOWN_INTERFACE_ERROR,
            UNKNOWN_INTERFACE_TEXT,
        )
        .is_ok());

        let (mut trailing, _server) =
            scripted_remote_error(":1.10", UNKNOWN_INTERFACE_ERROR, "ss", |writer| {
                writer.string(UNKNOWN_INTERFACE_TEXT)?;
                writer.string("unaccounted")
            });
        let outcome = unavailable_get_call(&mut trailing, interface).unwrap();
        assert!(require_exact_remote_error(
            outcome,
            ":1.10",
            interface,
            "Properties.Get",
            UNKNOWN_INTERFACE_ERROR,
            UNKNOWN_INTERFACE_TEXT,
        )
        .is_err());

        for (sender, name, text) in [
            (":1.11", UNKNOWN_INTERFACE_ERROR, UNKNOWN_INTERFACE_TEXT),
            (":1.10", UNKNOWN_METHOD_ERROR, UNKNOWN_INTERFACE_TEXT),
            (":1.10", UNKNOWN_INTERFACE_ERROR, "another refusal"),
        ] {
            let (mut wrong, _server) =
                scripted_remote_error(sender, name, "s", |writer| writer.string(text));
            let outcome = unavailable_get_call(&mut wrong, interface).unwrap();
            assert!(require_exact_remote_error(
                outcome,
                ":1.10",
                interface,
                "Properties.Get",
                UNKNOWN_INTERFACE_ERROR,
                UNKNOWN_INTERFACE_TEXT,
            )
            .is_err());
        }
    }

    #[test]
    fn peer_error_text_cannot_inject_a_trusted_console_line() {
        let interface = UNAVAILABLE_INTERFACES[0].0;
        let injected = format!("wrong\nportal-evidence: {UNAVAILABLE_READY_MARKER}");
        let (mut connection, _server) =
            scripted_remote_error(":1.10", UNKNOWN_INTERFACE_ERROR, "s", |writer| {
                writer.string(&injected)
            });
        let outcome = unavailable_get_call(&mut connection, interface).unwrap();
        let error = require_exact_remote_error(
            outcome,
            ":1.10",
            interface,
            "Properties.Get",
            UNKNOWN_INTERFACE_ERROR,
            UNKNOWN_INTERFACE_TEXT,
        )
        .unwrap_err();
        assert_eq!(error.lines().count(), 1);
        assert!(error.contains("wrong\\nportal-evidence:"));
        assert!(!error.contains(&format!("\nportal-evidence: {UNAVAILABLE_READY_MARKER}")));
    }

    #[test]
    fn the_probe_rejects_a_response_before_its_method_reply() {
        let path = "/org/freedesktop/portal/desktop/request/1_9/early";
        let bytes =
            message::Builder::signal(Endian::Little, path, handles::REQUEST_INTERFACE, "Response")
                .sender(":1.10")
                .destination(":1.9")
                .serial(4)
                .body("ua{sv}", |writer| write_background_response(writer, 2))
                .unwrap()
                .encode()
                .unwrap();
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        server_stream.write_all(&bytes).unwrap();
        let mut connection = Connection {
            stream: client_stream,
            serial: 0,
            unique: Some(":1.9".into()),
            until: Some(Instant::now() + Duration::from_secs(2)),
        };
        let error = connection
            .call_guarded(
                PORTAL_NAME,
                PORTAL_PATH,
                BACKGROUND_INTERFACE,
                "RequestBackground",
                CallGuard {
                    signature: "sa{sv}",
                    early_response_path: Some(path),
                },
                |writer| {
                    writer.string("")?;
                    writer.array("{sv}", |_| Ok(()))
                },
            )
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Request.Response arrived before its method reply"
        );
    }

    #[test]
    fn every_introspection_xml_declares_properties_changed_once() {
        const SIGNAL: &str = "<signal name=\"PropertiesChanged\">";
        for xml in [
            INTROSPECTION_XML,
            REQUEST_INTROSPECTION_XML,
            SESSION_INTROSPECTION_XML,
        ] {
            assert_eq!(xml.matches(SIGNAL).count(), 1, "{xml}");
        }
    }

    #[test]
    fn historic_read_wraps_the_concrete_setting_in_a_second_variant() {
        let settings = Settings::parse(settings::DEFAULT_CONFIG).unwrap();
        let bytes = call(SETTINGS_INTERFACE, "Read", "ss", |writer| {
            writer.string(APPEARANCE)?;
            writer.string("color-scheme")
        });
        let (request, _) = message::decode(&bytes, 0).unwrap();
        let reply = dispatch_one(&request, &settings, 12);
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
        let reply = dispatch_one(&request, &settings, 12);
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
        let reply = dispatch_one(&request, &settings, 13);
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
        let reply = dispatch_one(&request, &settings, 14);
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
        let mut handles = Handles::default();
        let mut serials = 9;
        let outbound = dispatch(&request, &settings, &mut handles, &mut serials).unwrap();
        assert!(outbound.frames.is_empty());
        assert!(outbound.retire.is_none());
    }
}
