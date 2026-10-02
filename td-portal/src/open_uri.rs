//! org.freedesktop.portal.OpenURI: an `http` or `https` link goes to the
//! running Firefox through its remote-control `OpenURL`; `file` and every
//! other scheme, and the descriptor-taking `OpenFile`, are refused
//! (APPLICATIONS.md §E row 4).
//!
//! The handler is found on the bus, not started: the portal lists the names
//! under Firefox's `org.mozilla.firefox.*` grant, resolves each one's owner
//! in turn, and calls the first owner the broker places in the Firefox jail
//! at Firefox's assigned UID. A same-uid process holding such a name outside
//! the jail is not the browser: it is passed over and never called.

use super::*;

pub(super) const INTERFACE: &str = "org.freedesktop.portal.OpenURI";
pub(super) const VERSION: u32 = 1;
pub(super) const UNSUPPORTED_FILE: &str =
    "td OpenURI does not open files: the handler runs in another sandbox";

const LIMIT: usize = 16;
const PER_OWNER: usize = 4;
const MAX_URI_BYTES: usize = 8192;
const MAX_PARENT_BYTES: usize = 256;
const MAX_LISTED_NAMES: usize = 4096;
/// Names under the grant tried before failing: a same-uid process may hold
/// one first, and is passed over rather than allowed to block the browser.
const MAX_CANDIDATES: usize = 8;
/// Browser calls outstanding at once, counting expired ones the browser has
/// not answered: the broker keeps each in its pending-reply table until an
/// answer or a departure, so a hung browser must not exhaust the portal's.
const MAX_BROWSER_CALLS: usize = 8;
const BROWSER: &str = "firefox";
const REMOTE_PREFIX: &str = "org.mozilla.firefox.";
const REMOTE_PATH: &str = "/org/mozilla/firefox/Remote";
const REMOTE_INTERFACE: &str = "org.mozilla.firefox";
const REMOTE_MEMBER: &str = "OpenURL";
const FAILED: u32 = 2;

/// Where a request is: each stage waits on one outstanding call.
#[derive(Debug, PartialEq)]
enum Stage {
    Caller,
    Names,
    Owner,
    Browser { unique: String },
    Open { unique: String },
}

pub(super) struct Pending {
    pub owner: String,
    path: String,
    endian: Endian,
    uri: String,
    https: bool,
    app: Option<String>,
    stage: Stage,
    /// Names still to try, the next last.
    candidates: Vec<String>,
    /// Whether a holder was passed over for failing authentication.
    impostor: bool,
    deadline: Instant,
}

pub(super) fn is_call(call: &Message<'_>) -> bool {
    call.kind == MessageType::MethodCall
        && call.fields.path == Some(PORTAL_PATH)
        && call.fields.interface == Some(INTERFACE)
        && call.fields.member == Some("OpenURI")
}

/// Whether `call` is the descriptor-taking member td does not serve.
pub(super) fn is_open_file(call: &Message<'_>) -> bool {
    call.kind == MessageType::MethodCall
        && call.fields.path == Some(PORTAL_PATH)
        && call.fields.interface == Some(INTERFACE)
        && call.fields.member == Some("OpenFile")
}

#[derive(Debug, PartialEq)]
struct Parsed<'a> {
    uri: &'a str,
    https: bool,
    token: Option<&'a str>,
}

/// Why a URI is not one this portal opens, as `(error, text)`.
type Refusal = (&'static str, &'static str);

fn refusal(text: &'static str) -> Refusal {
    ("org.freedesktop.DBus.Error.InvalidArgs", text)
}

/// The link `uri` names, when it is one the browser is handed: an `http` or
/// `https` scheme in any case, an authority after `//`, no whitespace or
/// control character, and a bounded length.
fn link(uri: &str) -> Result<bool, Refusal> {
    let (scheme, rest) = uri
        .split_once(':')
        .ok_or_else(|| refusal("the URI has no scheme"))?;
    let https = if scheme.eq_ignore_ascii_case("https") {
        true
    } else if scheme.eq_ignore_ascii_case("http") {
        false
    } else if scheme.eq_ignore_ascii_case("file") {
        return Err((UNSUPPORTED_OPEN, UNSUPPORTED_FILE));
    } else {
        return Err((UNSUPPORTED_OPEN, "td OpenURI opens only http and https"));
    };
    if uri.len() > MAX_URI_BYTES {
        return Err(refusal("the URI is longer than the portal admits"));
    }
    let authority = rest
        .strip_prefix("//")
        .map(|rest| rest.split(['/', '?', '#']).next().unwrap_or_default());
    if authority.is_none_or(str::is_empty)
        || uri.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(refusal("the URI is not one http or https link"));
    }
    Ok(https)
}

fn parse<'a>(call: &'a Message<'a>) -> Result<Parsed<'a>, Refusal> {
    if !exact_signature(call, "ssa{sv}") {
        return Err(refusal("OpenURI takes a parent, a URI and options"));
    }
    let [Value::Str(parent), Value::Str(uri), Value::Array(options)] = call.args() else {
        return Err(refusal("OpenURI takes a parent, a URI and options"));
    };
    if parent.len() > MAX_PARENT_BYTES {
        return Err(refusal("the parent window handle is too long"));
    }
    let https = link(uri)?;
    let entries = options
        .values(MAX_PORTAL_OPTIONS)
        .map_err(|_| refusal("OpenURI has too many options"))?;
    let mut token = None;
    for entry in &entries {
        let pair = entry
            .as_seq()
            .and_then(|pair| pair.values(2).ok())
            .ok_or_else(|| refusal("an OpenURI option is malformed"))?;
        let [Value::Str(key), Value::Variant(value)] = pair.as_slice() else {
            return Err(refusal("an OpenURI option has the wrong shape"));
        };
        let expected = match *key {
            "handle_token" | "activation_token" => "s",
            "writable" | "ask" => "b",
            // Unknown options are ignored, as the portal specification asks.
            _ => continue,
        };
        if value.signature() != expected {
            return Err(refusal("an OpenURI option has the wrong type"));
        }
        if *key != "handle_token" {
            continue;
        }
        let values = value
            .values(1)
            .map_err(|_| refusal("handle_token has a malformed variant"))?;
        let (Some(Value::Str(text)), None) = (values.first(), token) else {
            return Err(refusal("handle_token must appear once as a string"));
        };
        if !handles::valid_token(text) {
            return Err(refusal("handle_token is not a valid object-path element"));
        }
        token = Some(*text);
    }
    Ok(Parsed { uri, https, token })
}

fn bus_call(
    serial: u32,
    member: &str,
    signature: &str,
    argument: Option<&str>,
) -> io::Result<Vec<u8>> {
    let builder = message::Builder::method_call(Endian::Little, BUS_PATH, Some(BUS_NAME), member)
        .destination(BUS_NAME)
        .serial(serial);
    match argument {
        Some(argument) => builder
            .body(signature, |writer| writer.string(argument))
            .map_err(wire_error)?,
        None => builder,
    }
    .encode()
    .map_err(message_error)
}

/// Firefox's remote command line: little-endian `argc`, one offset per
/// argument from the buffer's start, the working directory, then the
/// NUL-terminated arguments (toolkit/components/remote, `OpenURL(ay)`).
fn command_line(uri: &str) -> Option<Vec<u8>> {
    let directory = b"/\0";
    let arguments: [&[u8]; 2] = [b"firefox", uri.as_bytes()];
    let header = 4usize.checked_mul(arguments.len().checked_add(1)?)?;
    let mut buffer = Vec::with_capacity(
        header
            .checked_add(directory.len())?
            .checked_add(uri.len())?
            .checked_add(16)?,
    );
    buffer.extend_from_slice(&i32::try_from(arguments.len()).ok()?.to_le_bytes());
    let mut at = header.checked_add(directory.len())?;
    for argument in arguments {
        buffer.extend_from_slice(&i32::try_from(at).ok()?.to_le_bytes());
        at = at.checked_add(argument.len())?.checked_add(1)?;
    }
    buffer.extend_from_slice(directory);
    for argument in arguments {
        buffer.extend_from_slice(argument);
        buffer.push(0);
    }
    Some(buffer)
}

fn open_call(serial: u32, unique: &str, uri: &str) -> io::Result<Vec<u8>> {
    let line =
        command_line(uri).ok_or_else(|| io::Error::other("the browser command line overflowed"))?;
    message::Builder::method_call(
        Endian::Little,
        REMOTE_PATH,
        Some(REMOTE_INTERFACE),
        REMOTE_MEMBER,
    )
    .destination(unique)
    .serial(serial)
    .body("ay", |writer| {
        writer.array("y", |writer| {
            writer.append(&line);
            Ok(())
        })
    })
    .map_err(wire_error)?
    .encode()
    .map_err(message_error)
}

fn refuse(
    connection: &mut Connection,
    call: &Message<'_>,
    owner: &str,
    (name, text): Refusal,
) -> io::Result<()> {
    if call.flags & message::FLAG_NO_REPLY_EXPECTED != 0 {
        return Ok(());
    }
    let serial = connection.next_serial()?;
    connection.write_frame(&method_error(
        call.endian,
        serial,
        call.serial,
        owner,
        name,
        text,
    )?)
}

/// `OpenFile` hands the portal a descriptor its handler could not reach.
pub(super) fn refuse_open_file(connection: &mut Connection, call: &Message<'_>) -> io::Result<()> {
    let owner = call
        .fields
        .sender
        .ok_or_else(|| io::Error::other("OpenFile has no broker sender"))?;
    refuse(
        connection,
        call,
        owner,
        (UNSUPPORTED_OPEN, UNSUPPORTED_FILE),
    )
}

pub(super) fn begin(
    connection: &mut Connection,
    state: &mut ServiceState,
    call: &Message<'_>,
) -> io::Result<()> {
    let owner = call
        .fields
        .sender
        .ok_or_else(|| io::Error::other("OpenURI has no broker sender"))?;
    let parsed = match parse(call) {
        Ok(parsed) => parsed,
        Err(why) => return refuse(connection, call, owner, why),
    };
    if state.open_uris.len() >= LIMIT
        || state
            .open_uris
            .values()
            .filter(|pending| pending.owner == owner)
            .count()
            >= PER_OWNER
    {
        return refuse(
            connection,
            call,
            owner,
            (
                "org.freedesktop.DBus.Error.LimitsExceeded",
                "too many OpenURI requests are pending",
            ),
        );
    }
    let token = parsed
        .token
        .map_or_else(|| format!("td_{}", call.serial), str::to_string);
    let path = match state.handles.reserve(HandleKind::Request, owner, &token) {
        Ok(path) => path,
        Err(ReserveError::InvalidOwner) => {
            return Err(io::Error::other(
                "the broker assigned a sender outside td-busd's unique-name shape",
            ))
        }
        Err(ReserveError::InvalidToken) => {
            return refuse(
                connection,
                call,
                owner,
                refusal("handle_token is not a valid object-path element"),
            )
        }
        Err(ReserveError::Duplicate) => {
            return refuse(
                connection,
                call,
                owner,
                (
                    "org.freedesktop.portal.Error.Exists",
                    "that request handle is already active",
                ),
            )
        }
        Err(ReserveError::OwnerFull | ReserveError::Full) => {
            return refuse(
                connection,
                call,
                owner,
                (
                    "org.freedesktop.DBus.Error.LimitsExceeded",
                    "this portal has too many active handles",
                ),
            )
        }
    };
    if call.flags & message::FLAG_NO_REPLY_EXPECTED == 0 {
        let serial = connection.next_serial()?;
        connection.write_frame(&method_return(
            call.endian,
            serial,
            call.serial,
            owner,
            "o",
            |writer| writer.object_path(&path),
        )?)?;
    }
    let serial = connection.next_serial()?;
    let query = credentials_query(serial, owner)?;
    state.open_uris.insert(
        serial,
        Pending {
            owner: owner.to_string(),
            path,
            endian: call.endian,
            uri: parsed.uri.to_string(),
            https: parsed.https,
            app: None,
            stage: Stage::Caller,
            candidates: Vec::new(),
            impostor: false,
            deadline: Instant::now() + EXCHANGE_TIMEOUT,
        },
    );
    connection.write_frame(&query)
}

/// The held names under Firefox's remote-control grant, in order, at most
/// `MAX_CANDIDATES` of them.
fn remote_names(reply: &Message<'_>) -> Vec<String> {
    let [Value::Array(names)] = reply.args() else {
        return Vec::new();
    };
    let mut held = names
        .values(MAX_LISTED_NAMES)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|name| name.as_str())
        .filter(|name| {
            name.strip_prefix(REMOTE_PREFIX)
                .is_some_and(|rest| !rest.is_empty())
                && name::valid_well_known_name(name)
        })
        .map(str::to_string)
        .collect::<Vec<_>>();
    held.sort_unstable();
    held.truncate(MAX_CANDIDATES);
    held
}

/// Whether `reply` answers `pending`'s outstanding call from the peer it
/// was sent to. At `OpenURL` the bus may answer for a browser that left.
fn answers(connection: &Connection, pending: &Pending, reply: &Message<'_>) -> bool {
    let from_bus = reply.fields.sender == Some(BUS_NAME);
    let from = match &pending.stage {
        Stage::Open { unique } => {
            reply.fields.sender == Some(unique.as_str())
                || (from_bus && reply.kind == MessageType::Error)
        }
        _ => from_bus,
    };
    from && matches!(reply.kind, MessageType::MethodReturn | MessageType::Error)
        && reply.fields.destination == connection.unique.as_deref()
}

/// Asks who holds the next candidate name, or fails the request when none
/// is left.
fn next_candidate(
    connection: &mut Connection,
    state: &mut ServiceState,
    mut pending: Pending,
) -> io::Result<bool> {
    let Some(name) = pending.candidates.pop() else {
        let outcome = if pending.impostor {
            "unauthenticated-browser"
        } else {
            "no-browser"
        };
        return finish(connection, state, pending, FAILED, outcome);
    };
    let serial = connection.next_serial()?;
    let call = bus_call(serial, "GetNameOwner", "s", Some(&name))?;
    pending.stage = Stage::Owner;
    state.open_uris.insert(serial, pending);
    connection.write_frame(&call)?;
    Ok(true)
}

pub(super) fn reply(
    connection: &mut Connection,
    state: &mut ServiceState,
    reply: &Message<'_>,
) -> io::Result<bool> {
    let Some(serial) = reply.fields.reply_serial else {
        return Ok(false);
    };
    if let Some(unique) = state.open_uri_stale.get(&serial) {
        let late = reply.fields.sender == Some(unique.as_str())
            || (reply.fields.sender == Some(BUS_NAME) && reply.kind == MessageType::Error);
        if late && reply.fields.destination == connection.unique.as_deref() {
            state.open_uri_stale.remove(&serial);
        }
        return Ok(true);
    }
    let Some(pending) = state.open_uris.get(&serial) else {
        return Ok(false);
    };
    if !answers(connection, pending, reply) {
        return Ok(true);
    }
    let mut pending = state
        .open_uris
        .remove(&serial)
        .ok_or_else(|| io::Error::other("an OpenURI request disappeared"))?;
    let failed = reply.kind == MessageType::Error;
    let (next, call) = match std::mem::replace(&mut pending.stage, Stage::Caller) {
        Stage::Caller => {
            pending.app = if failed {
                None
            } else {
                credentials_app_id(reply, state.application_policy.as_ref())
                    .ok()
                    .flatten()
            };
            let serial = connection.next_serial()?;
            (
                Stage::Names,
                (serial, bus_call(serial, "ListNames", "", None)?),
            )
        }
        Stage::Names => {
            pending.candidates = if failed {
                Vec::new()
            } else {
                remote_names(reply)
            };
            pending.candidates.reverse();
            return next_candidate(connection, state, pending);
        }
        Stage::Owner => {
            let unique = match reply.args() {
                [Value::Str(unique)] if !failed && name::valid_unique_name(unique) => {
                    (*unique).to_string()
                }
                // The holder left between the listing and the lookup.
                _ => return next_candidate(connection, state, pending),
            };
            let serial = connection.next_serial()?;
            let call = credentials_query(serial, &unique)?;
            (Stage::Browser { unique }, (serial, call))
        }
        Stage::Browser { unique } => {
            if failed {
                return next_candidate(connection, state, pending);
            }
            let browser = credentials_app_id(reply, state.application_policy.as_ref())
                .ok()
                .flatten()
                .as_deref()
                == Some(BROWSER);
            if !browser {
                pending.impostor = true;
                return next_candidate(connection, state, pending);
            }
            if browser_calls(state) >= MAX_BROWSER_CALLS {
                return finish(connection, state, pending, FAILED, "browser-unresponsive");
            }
            let serial = connection.next_serial()?;
            let call = open_call(serial, &unique, &pending.uri)?;
            (Stage::Open { unique }, (serial, call))
        }
        Stage::Open { .. } => {
            return if failed {
                finish(connection, state, pending, FAILED, "browser-refused")
            } else {
                finish(connection, state, pending, 0, "opened")
            };
        }
    };
    pending.stage = next;
    let (serial, frame) = call;
    state.open_uris.insert(serial, pending);
    connection.write_frame(&frame)?;
    Ok(true)
}

/// Drops every request on `path`, which its caller closed, so none sends
/// its link after the close or answers a later request on the same path.
pub(super) fn close(state: &mut ServiceState, path: &str) {
    abandon(state, |pending| pending.path == path, 1, "closed");
}

/// Drops every request whose caller left the bus, and forgets expired
/// calls to a browser that left, which the broker no longer holds.
pub(super) fn depart(state: &mut ServiceState, owner: &str) {
    abandon(
        state,
        |pending| pending.owner == owner,
        FAILED,
        "caller-departed",
    );
    state.open_uri_stale.retain(|_, unique| unique != owner);
}

/// Drops the requests `abandoned` selects. A browser call already sent
/// stays counted as stale: the broker holds it until the browser answers.
fn abandon(
    state: &mut ServiceState,
    abandoned: impl Fn(&Pending) -> bool,
    response: u32,
    outcome: &str,
) {
    let serials = state
        .open_uris
        .iter()
        .filter_map(|(serial, pending)| abandoned(pending).then_some(*serial))
        .collect::<Vec<_>>();
    for serial in serials {
        if let Some(pending) = state.open_uris.remove(&serial) {
            eprintln!("{}", record(&pending, response, outcome));
            if let Stage::Open { unique } = pending.stage {
                state.open_uri_stale.insert(serial, unique);
            }
        }
    }
}

/// Answers `pending` with `response` unless its caller closed the request
/// or left, and records the outcome without the link itself.
fn finish(
    connection: &mut Connection,
    state: &mut ServiceState,
    pending: Pending,
    response: u32,
    outcome: &str,
) -> io::Result<bool> {
    eprintln!("{}", record(&pending, response, outcome));
    if state.handles.retire(&pending.path) {
        let serial = connection.next_serial()?;
        connection.write_frame(&response_signal(
            pending.endian,
            serial,
            &pending.path,
            &pending.owner,
            response,
        )?)?;
    }
    Ok(true)
}

fn record(pending: &Pending, response: u32, outcome: &str) -> String {
    format!(
        "TD-PORTAL-OPEN-URI app={} scheme={} response={response} outcome={outcome}",
        pending.app.as_deref().unwrap_or("-"),
        if pending.https { "https" } else { "http" },
    )
}

fn response_signal(
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
            writer.uint32(response);
            writer.array("{sv}", |_| Ok(()))
        })
        .map_err(wire_error)?
        .encode()
        .map_err(message_error)
}

/// The browser calls the broker is holding for the portal: live ones and
/// expired ones still unanswered.
fn browser_calls(state: &ServiceState) -> usize {
    state
        .open_uris
        .values()
        .filter(|pending| matches!(pending.stage, Stage::Open { .. }))
        .count()
        .saturating_add(state.open_uri_stale.len())
}

/// Fails every request whose exchange outlived its deadline. An unanswered
/// browser call is remembered until the browser answers or leaves.
pub(super) fn expire(connection: &mut Connection, state: &mut ServiceState) -> io::Result<()> {
    let now = Instant::now();
    let expired = state
        .open_uris
        .iter()
        .filter_map(|(serial, pending)| (pending.deadline <= now).then_some(*serial))
        .collect::<Vec<_>>();
    for serial in expired {
        if let Some(pending) = state.open_uris.remove(&serial) {
            if let Stage::Open { unique } = &pending.stage {
                state.open_uri_stale.insert(serial, unique.clone());
            }
            finish(connection, state, pending, FAILED, "timed-out")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(signature: &str, fill: impl FnOnce(&mut Writer) -> Result<(), WireError>) -> Vec<u8> {
        message::Builder::method_call(Endian::Little, PORTAL_PATH, Some(INTERFACE), "OpenURI")
            .destination(PORTAL_NAME)
            .sender(":1.7")
            .serial(9)
            .body(signature, fill)
            .unwrap()
            .encode()
            .unwrap()
    }

    fn open(uri: &str, token: Option<&str>) -> Vec<u8> {
        call("ssa{sv}", |writer| {
            writer.string("")?;
            writer.string(uri)?;
            writer.array("{sv}", |writer| {
                if let Some(token) = token {
                    writer.dict_entry(|writer| {
                        writer.string("handle_token")?;
                        writer.variant("s", |writer| writer.string(token))
                    })?;
                }
                writer.dict_entry(|writer| {
                    writer.string("unknown-future-option")?;
                    writer.variant("u", |writer| {
                        writer.uint32(1);
                        Ok(())
                    })
                })
            })
        })
    }

    fn parsed(frame: &[u8]) -> Result<(String, bool, Option<String>), Refusal> {
        let (message, _) = message::decode(frame, 0).unwrap();
        parse(&message).map(|parsed| {
            (
                parsed.uri.to_string(),
                parsed.https,
                parsed.token.map(str::to_string),
            )
        })
    }

    #[test]
    fn only_one_http_or_https_link_is_handed_on() {
        assert_eq!(
            parsed(&open("https://example.org/a?b=c#d", Some("t1"))),
            Ok((
                "https://example.org/a?b=c#d".into(),
                true,
                Some("t1".into())
            ))
        );
        assert_eq!(
            parsed(&open("HTTP://example.org/", None)),
            Ok(("HTTP://example.org/".into(), false, None))
        );
        for (uri, error) in [
            ("file:///etc/passwd", UNSUPPORTED_OPEN),
            ("FILE:///tmp/x", UNSUPPORTED_OPEN),
            ("mailto:a@example.org", UNSUPPORTED_OPEN),
            ("javascript:alert(1)", UNSUPPORTED_OPEN),
            (
                "https:example.org",
                "org.freedesktop.DBus.Error.InvalidArgs",
            ),
            ("https://", "org.freedesktop.DBus.Error.InvalidArgs"),
            ("https://a b", "org.freedesktop.DBus.Error.InvalidArgs"),
            ("https://a\n", "org.freedesktop.DBus.Error.InvalidArgs"),
            ("-https://a", UNSUPPORTED_OPEN),
            ("no scheme", "org.freedesktop.DBus.Error.InvalidArgs"),
        ] {
            assert_eq!(parsed(&open(uri, None)).unwrap_err().0, error, "{uri:?}");
        }
        let long = format!("https://e.example/{}", "a".repeat(MAX_URI_BYTES));
        assert!(parsed(&open(&long, None)).is_err());
        assert_eq!(
            parsed(&open("file:///x", None)).unwrap_err().1,
            UNSUPPORTED_FILE
        );
    }

    #[test]
    fn options_are_typed_and_the_token_is_one_path_element() {
        for token in ["", "a/b", "a-b", &"t".repeat(65)] {
            assert!(parsed(&open("https://e.example/", Some(token))).is_err());
        }
        let mistyped = call("ssa{sv}", |writer| {
            writer.string("")?;
            writer.string("https://e.example/")?;
            writer.array("{sv}", |writer| {
                writer.dict_entry(|writer| {
                    writer.string("ask")?;
                    writer.variant("s", |writer| writer.string("yes"))
                })
            })
        });
        assert!(parsed(&mistyped).is_err());
        let wrong = call("ss", |writer| {
            writer.string("")?;
            writer.string("https://e.example/")
        });
        assert!(parsed(&wrong).is_err());
    }

    /// The exact bytes Firefox's remote server decodes: argc, offsets from
    /// the buffer start, the working directory, then each argument.
    #[test]
    fn the_remote_command_line_is_firefoxs_layout() {
        let line = command_line("https://e.example/").unwrap();
        let mut expected = Vec::new();
        expected.extend_from_slice(&2i32.to_le_bytes());
        expected.extend_from_slice(&14i32.to_le_bytes());
        expected.extend_from_slice(&22i32.to_le_bytes());
        expected.extend_from_slice(b"/\0firefox\0https://e.example/\0");
        assert_eq!(line, expected);
        let frame = open_call(5, ":1.9", "https://e.example/").unwrap();
        let (message, _) = message::decode(&frame, 0).unwrap();
        assert_eq!(message.fields.destination, Some(":1.9"));
        assert_eq!(message.fields.path, Some(REMOTE_PATH));
        assert_eq!(message.fields.interface, Some(REMOTE_INTERFACE));
        assert_eq!(message.fields.member, Some(REMOTE_MEMBER));
        assert_eq!(message.fields.signature, Some("ay"));
        let [Value::Array(bytes)] = message.args() else {
            panic!("OpenURL carries one byte array");
        };
        assert_eq!(bytes.bytes(), expected.as_slice());
    }

    #[test]
    fn the_browser_is_the_first_name_under_its_grant() {
        let names = |names: &[&str]| {
            message::Builder::method_return(Endian::Little, 3)
                .sender(BUS_NAME)
                .serial(4)
                .body("as", |writer| {
                    writer.array("s", |writer| {
                        for name in names {
                            writer.string(name)?;
                        }
                        Ok(())
                    })
                })
                .unwrap()
                .encode()
                .unwrap()
        };
        let pick = |list: &[&str]| {
            let frame = names(list);
            let (message, _) = message::decode(&frame, 0).unwrap();
            remote_names(&message)
        };
        assert_eq!(
            pick(&[
                ":1.4",
                "org.mozilla.firefox",
                "org.mozilla.firefox.b",
                "org.mozilla.firefox.a",
                "org.freedesktop.portal.Desktop",
            ]),
            ["org.mozilla.firefox.a", "org.mozilla.firefox.b"]
        );
        assert!(pick(&["org.mozilla.firefox", "org.mozilla.firefoxx.a"]).is_empty());
        let many = (0..20)
            .map(|index| format!("org.mozilla.firefox.n{index:02}"))
            .collect::<Vec<_>>();
        let many = many.iter().map(String::as_str).collect::<Vec<_>>();
        assert_eq!(pick(&many).len(), MAX_CANDIDATES);
    }

    struct Broker {
        connection: Connection,
        broker: UnixStream,
        state: ServiceState,
    }

    impl Broker {
        fn new() -> Self {
            let (service, broker) = UnixStream::pair().unwrap();
            broker
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            Self {
                connection: Connection {
                    stream: service,
                    serial: 40,
                    unique: Some(":1.10".into()),
                    until: None,
                    setup_events: Vec::new(),
                },
                broker,
                state: crate::tests::service_state(),
            }
        }

        fn next(&mut self) -> Vec<u8> {
            let IncomingFrame::Message(frame) = read_frame(&mut self.broker).unwrap() else {
                panic!("the portal wrote no complete frame");
            };
            frame
        }

        /// Answers the portal's last call from `sender`, returning the
        /// portal's next frame when it wrote one.
        fn answer(
            &mut self,
            call: &[u8],
            sender: &str,
            error: bool,
            signature: &str,
            fill: impl FnOnce(&mut Writer) -> Result<(), WireError>,
        ) {
            let (call, _) = message::decode(call, 0).unwrap();
            let builder = if error {
                message::Builder::error(Endian::Little, "org.example.Error.Failed", call.serial)
            } else {
                message::Builder::method_return(Endian::Little, call.serial)
            };
            let frame = builder
                .sender(sender)
                .destination(":1.10")
                .serial(900)
                .body(signature, fill)
                .unwrap()
                .encode()
                .unwrap();
            let (reply_message, _) = message::decode(&frame, 0).unwrap();
            assert!(reply(&mut self.connection, &mut self.state, &reply_message).unwrap());
        }
    }

    impl Broker {
        /// Delivers an empty return the portal should take silently.
        fn answer_quietly_empty(&mut self, call: &[u8], sender: &str) {
            let (call, _) = message::decode(call, 0).unwrap();
            let frame = message::Builder::method_return(Endian::Little, call.serial)
                .sender(sender)
                .destination(":1.10")
                .serial(902)
                .encode()
                .unwrap();
            let (reply_message, _) = message::decode(&frame, 0).unwrap();
            assert!(reply(&mut self.connection, &mut self.state, &reply_message).unwrap());
        }

        /// Delivers a bus reply the portal should take and say nothing to.
        fn answer_quietly(
            &mut self,
            call: &[u8],
            sender: &str,
            fill: impl FnOnce(&mut Writer) -> Result<(), WireError>,
        ) {
            let (call, _) = message::decode(call, 0).unwrap();
            let frame = message::Builder::method_return(Endian::Little, call.serial)
                .sender(sender)
                .destination(":1.10")
                .serial(901)
                .body("a{sv}", fill)
                .unwrap()
                .encode()
                .unwrap();
            let (reply_message, _) = message::decode(&frame, 0).unwrap();
            assert!(!reply(&mut self.connection, &mut self.state, &reply_message).unwrap());
        }
    }

    fn credentials(
        uid: u32,
        app: Option<&str>,
    ) -> impl FnOnce(&mut Writer) -> Result<(), WireError> + '_ {
        move |writer| {
            writer.array("{sv}", |writer| {
                writer.dict_entry(|writer| {
                    writer.string("UnixUserID")?;
                    writer.variant("u", |writer| {
                        writer.uint32(uid);
                        Ok(())
                    })
                })?;
                if let Some(app) = app {
                    writer.dict_entry(|writer| {
                        writer.string("td.AppId")?;
                        writer.variant("s", |writer| writer.string(app))
                    })?;
                }
                Ok(())
            })
        }
    }

    fn member(frame: &[u8]) -> (String, Option<String>) {
        let (message, _) = message::decode(frame, 0).unwrap();
        (
            message.fields.member.unwrap_or_default().to_string(),
            message.fields.destination.map(str::to_string),
        )
    }

    /// Drives one request from the call to the browser's question.
    fn to_browser(broker: &mut Broker, browser_uid: u32, browser: Option<&str>) -> Vec<u8> {
        let frame = open("https://e.example/a", Some("t1"));
        let (call, _) = message::decode(&frame, 0).unwrap();
        begin(&mut broker.connection, &mut broker.state, &call).unwrap();
        let handle = broker.next();
        let (handle, _) = message::decode(&handle, 0).unwrap();
        assert_eq!(
            handle.args(),
            vec![Value::ObjectPath(
                "/org/freedesktop/portal/desktop/request/1_7/t1"
            )]
        );
        let query = broker.next();
        assert_eq!(member(&query).0, "GetConnectionCredentials");
        broker.answer(
            &query,
            BUS_NAME,
            false,
            "a{sv}",
            credentials(65538, Some("news")),
        );
        let list = broker.next();
        assert_eq!(member(&list).0, "ListNames");
        broker.answer(&list, BUS_NAME, false, "as", |writer| {
            writer.array("s", |writer| {
                for name in [":1.9", "org.mozilla.firefox", "org.mozilla.firefox.p"] {
                    writer.string(name)?;
                }
                Ok(())
            })
        });
        let owner = broker.next();
        let (owner_call, _) = message::decode(&owner, 0).unwrap();
        assert_eq!(owner_call.fields.member, Some("GetNameOwner"));
        assert_eq!(owner_call.args(), vec![Value::Str("org.mozilla.firefox.p")]);
        broker.answer(&owner, BUS_NAME, false, "s", |writer| writer.string(":1.9"));
        let identity = broker.next();
        let (identity_call, _) = message::decode(&identity, 0).unwrap();
        assert_eq!(identity_call.args(), vec![Value::Str(":1.9")]);
        broker.answer(
            &identity,
            BUS_NAME,
            false,
            "a{sv}",
            credentials(browser_uid, browser),
        );
        broker.next()
    }

    fn response(frame: &[u8]) -> u32 {
        let (signal, _) = message::decode(frame, 0).unwrap();
        assert_eq!(signal.kind, MessageType::Signal);
        assert_eq!(signal.fields.member, Some("Response"));
        assert_eq!(signal.fields.destination, Some(":1.7"));
        assert_eq!(
            signal.fields.path,
            Some("/org/freedesktop/portal/desktop/request/1_7/t1")
        );
        let [Value::Uint32(code), Value::Array(_)] = signal.args() else {
            panic!("Response carries a code and results");
        };
        *code
    }

    #[test]
    fn a_link_reaches_the_authenticated_browser_and_answers_success() {
        let mut broker = Broker::new();
        let opened = to_browser(&mut broker, 65536, Some("firefox"));
        assert_eq!(
            member(&opened),
            ("OpenURL".to_string(), Some(":1.9".to_string()))
        );
        // A reply from anyone but the browser does not settle the request.
        broker.answer(&opened, BUS_NAME, false, "", |_| Ok(()));
        assert_eq!(broker.state.open_uris.len(), 1);
        broker.answer(&opened, ":1.9", false, "", |_| Ok(()));
        assert_eq!(response(&broker.next()), 0);
        assert!(broker.state.open_uris.is_empty());
    }

    #[test]
    fn a_name_holder_outside_the_firefox_jail_is_never_called() {
        for (uid, app) in [
            (1000, None),
            (65538, Some("news")),
            (65537, Some("firefox")),
        ] {
            let mut broker = Broker::new();
            assert_eq!(response(&to_browser(&mut broker, uid, app)), 2, "{uid}");
            assert!(broker.state.open_uris.is_empty());
        }
    }

    #[test]
    fn a_refusing_browser_or_an_absent_one_answers_failure() {
        let mut broker = Broker::new();
        let opened = to_browser(&mut broker, 65536, Some("firefox"));
        broker.answer(&opened, ":1.9", true, "", |_| Ok(()));
        assert_eq!(response(&broker.next()), 2);

        let mut broker = Broker::new();
        let frame = open("https://e.example/a", Some("t1"));
        let (call, _) = message::decode(&frame, 0).unwrap();
        begin(&mut broker.connection, &mut broker.state, &call).unwrap();
        let _handle = broker.next();
        let query = broker.next();
        broker.answer(&query, BUS_NAME, false, "a{sv}", credentials(1000, None));
        let list = broker.next();
        broker.answer(&list, BUS_NAME, false, "as", |writer| {
            writer.array("s", |writer| writer.string("org.mozilla.firefox"))
        });
        assert_eq!(response(&broker.next()), 2);
    }

    #[test]
    fn a_closed_request_is_not_answered_and_a_refused_scheme_reserves_nothing() {
        let mut broker = Broker::new();
        let opened = to_browser(&mut broker, 65536, Some("firefox"));
        assert!(broker
            .state
            .handles
            .retire("/org/freedesktop/portal/desktop/request/1_7/t1"));
        broker.answer(&opened, ":1.9", false, "", |_| Ok(()));
        broker
            .broker
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        assert!(read_frame(&mut broker.broker).is_err());

        let mut broker = Broker::new();
        let frame = open("file:///etc/passwd", Some("t2"));
        let (call, _) = message::decode(&frame, 0).unwrap();
        begin(&mut broker.connection, &mut broker.state, &call).unwrap();
        let refusal = broker.next();
        let (error, _) = message::decode(&refusal, 0).unwrap();
        assert_eq!(error.fields.error_name, Some(UNSUPPORTED_OPEN));
        assert!(broker.state.open_uris.is_empty());
        assert!(!broker
            .state
            .handles
            .retire("/org/freedesktop/portal/desktop/request/1_7/t2"));
    }

    /// Starts a request and answers up to the name listing with `names`.
    fn listed(broker: &mut Broker, token: &str, names: &[&str]) {
        let frame = open("https://e.example/a", Some(token));
        let (call, _) = message::decode(&frame, 0).unwrap();
        begin(&mut broker.connection, &mut broker.state, &call).unwrap();
        let _handle = broker.next();
        let query = broker.next();
        broker.answer(
            &query,
            BUS_NAME,
            false,
            "a{sv}",
            credentials(65538, Some("news")),
        );
        let list = broker.next();
        broker.answer(&list, BUS_NAME, false, "as", |writer| {
            writer.array("s", |writer| {
                for name in names {
                    writer.string(name)?;
                }
                Ok(())
            })
        });
    }

    #[test]
    fn a_same_uid_holder_of_an_earlier_name_is_passed_over() {
        let mut broker = Broker::new();
        listed(
            &mut broker,
            "t1",
            &["org.mozilla.firefox.A", "org.mozilla.firefox.p"],
        );
        let owner = broker.next();
        let (owner_call, _) = message::decode(&owner, 0).unwrap();
        assert_eq!(owner_call.args(), vec![Value::Str("org.mozilla.firefox.A")]);
        broker.answer(&owner, BUS_NAME, false, "s", |writer| writer.string(":1.8"));
        let identity = broker.next();
        broker.answer(&identity, BUS_NAME, false, "a{sv}", credentials(1000, None));
        let owner = broker.next();
        let (owner_call, _) = message::decode(&owner, 0).unwrap();
        assert_eq!(owner_call.args(), vec![Value::Str("org.mozilla.firefox.p")]);
        broker.answer(&owner, BUS_NAME, false, "s", |writer| writer.string(":1.9"));
        let identity = broker.next();
        broker.answer(
            &identity,
            BUS_NAME,
            false,
            "a{sv}",
            credentials(65536, Some("firefox")),
        );
        assert_eq!(
            member(&broker.next()),
            ("OpenURL".to_string(), Some(":1.9".to_string()))
        );
    }

    #[test]
    fn a_browser_that_leaves_mid_call_fails_at_once_through_the_bus() {
        let mut broker = Broker::new();
        let opened = to_browser(&mut broker, 65536, Some("firefox"));
        // A return from the bus is not the browser's answer; its error is.
        broker.answer(&opened, BUS_NAME, false, "", |_| Ok(()));
        assert_eq!(broker.state.open_uris.len(), 1);
        broker.answer(&opened, BUS_NAME, true, "", |_| Ok(()));
        assert_eq!(response(&broker.next()), 2);
    }

    #[test]
    fn a_late_success_is_still_the_answer() {
        let mut broker = Broker::new();
        let opened = to_browser(&mut broker, 65536, Some("firefox"));
        for pending in broker.state.open_uris.values_mut() {
            pending.deadline = Instant::now() - Duration::from_secs(1);
        }
        broker.answer(&opened, ":1.9", false, "", |_| Ok(()));
        assert_eq!(response(&broker.next()), 0);
    }

    #[test]
    fn an_expired_browser_call_counts_until_the_browser_answers_or_leaves() {
        let mut broker = Broker::new();
        let opened = to_browser(&mut broker, 65536, Some("firefox"));
        for pending in broker.state.open_uris.values_mut() {
            pending.deadline = Instant::now() - Duration::from_secs(1);
        }
        expire(&mut broker.connection, &mut broker.state).unwrap();
        assert_eq!(response(&broker.next()), 2);
        assert_eq!(broker.state.open_uri_stale.len(), 1);
        // The browser's eventual answer releases its slot.
        broker.answer(&opened, ":1.9", false, "", |_| Ok(()));
        assert!(broker.state.open_uri_stale.is_empty());
        // At the ceiling a request fails without calling the browser.
        for serial in 0..MAX_BROWSER_CALLS as u32 {
            broker
                .state
                .open_uri_stale
                .insert(1000 + serial, ":1.9".into());
        }
        let refused = to_browser(&mut broker, 65536, Some("firefox"));
        assert_eq!(response(&refused), 2);
        depart(&mut broker.state, ":1.9");
        assert!(broker.state.open_uri_stale.is_empty());
    }

    fn service_frame(broker: &mut Broker, frame: IncomingFrame) {
        let settings = Settings::parse(settings::DEFAULT_CONFIG).unwrap();
        let (events, _receiver) = mpsc::sync_channel(4);
        consume_bus_frame(
            &mut broker.connection,
            &settings,
            &mut broker.state,
            &events,
            frame,
        )
        .unwrap();
    }

    #[test]
    fn a_browser_call_abandoned_by_its_caller_stays_counted() {
        let mut broker = Broker::new();
        let opened = to_browser(&mut broker, 65536, Some("firefox"));
        close(
            &mut broker.state,
            "/org/freedesktop/portal/desktop/request/1_7/t1",
        );
        assert!(broker.state.open_uris.is_empty());
        assert_eq!(broker.state.open_uri_stale.len(), 1);
        broker.answer_quietly_empty(&opened, ":1.9");
        assert!(broker.state.open_uri_stale.is_empty());

        let mut broker = Broker::new();
        let _opened = to_browser(&mut broker, 65536, Some("firefox"));
        depart(&mut broker.state, ":1.7");
        assert!(broker.state.open_uris.is_empty());
        assert_eq!(broker.state.open_uri_stale.len(), 1);
        // The browser leaving releases the call too.
        depart(&mut broker.state, ":1.9");
        assert!(broker.state.open_uri_stale.is_empty());
    }

    #[test]
    fn a_closed_request_stops_and_its_token_can_be_used_again() {
        let mut broker = Broker::new();
        let frame = open("https://e.example/a", Some("t1"));
        service_frame(&mut broker, IncomingFrame::Message(frame.clone()));
        let _handle = broker.next();
        let query = broker.next();
        let close = message::Builder::method_call(
            Endian::Little,
            "/org/freedesktop/portal/desktop/request/1_7/t1",
            Some(handles::REQUEST_INTERFACE),
            "Close",
        )
        .destination(PORTAL_NAME)
        .sender(":1.7")
        .serial(31)
        .encode()
        .unwrap();
        service_frame(&mut broker, IncomingFrame::Message(close));
        let _closed = broker.next();
        assert!(broker.state.open_uris.is_empty());
        // The old exchange's answer reaches nothing.
        broker.answer_quietly(&query, BUS_NAME, credentials(65538, Some("news")));
        // The same token starts a fresh request.
        service_frame(&mut broker, IncomingFrame::Message(frame));
        let handle = broker.next();
        let (handle, _) = message::decode(&handle, 0).unwrap();
        assert_eq!(handle.kind, MessageType::MethodReturn);
        assert_eq!(broker.state.open_uris.len(), 1);
    }

    #[test]
    fn a_departed_caller_leaves_no_request_behind() {
        let mut broker = Broker::new();
        listed(&mut broker, "t1", &["org.mozilla.firefox.p"]);
        let _owner = broker.next();
        let signal =
            message::Builder::signal(Endian::Little, BUS_PATH, BUS_NAME, "NameOwnerChanged")
                .sender(BUS_NAME)
                .serial(50)
                .body("sss", |writer| {
                    writer.string(":1.7")?;
                    writer.string(":1.7")?;
                    writer.string("")
                })
                .unwrap()
                .encode()
                .unwrap();
        service_frame(&mut broker, IncomingFrame::Message(signal));
        assert!(broker.state.open_uris.is_empty());
    }

    #[test]
    fn open_file_is_not_supported_with_or_without_a_descriptor() {
        for descriptors in [0u32, 1] {
            let mut broker = Broker::new();
            let mut builder = message::Builder::method_call(
                Endian::Little,
                PORTAL_PATH,
                Some(INTERFACE),
                "OpenFile",
            )
            .destination(PORTAL_NAME)
            .sender(":1.7")
            .serial(32);
            if descriptors == 1 {
                builder = builder.unix_fds(1);
            }
            let bytes = if descriptors == 1 {
                builder.body("sha{sv}", |writer| {
                    writer.string("")?;
                    writer.unix_fd(0);
                    writer.array("{sv}", |_| Ok(()))
                })
            } else {
                builder.body("sa{sv}", |writer| {
                    writer.string("")?;
                    writer.array("{sv}", |_| Ok(()))
                })
            }
            .unwrap()
            .encode()
            .unwrap();
            let frame = if descriptors == 1 {
                IncomingFrame::Descriptors {
                    bytes,
                    count: descriptors,
                }
            } else {
                IncomingFrame::Message(bytes)
            };
            service_frame(&mut broker, frame);
            let error = broker.next();
            let (error, _) = message::decode(&error, 0).unwrap();
            assert_eq!(
                error.fields.error_name,
                Some(UNSUPPORTED_OPEN),
                "{descriptors}"
            );
            assert!(broker.state.open_uris.is_empty());
        }
    }

    #[test]
    fn the_interface_publishes_version_one() {
        assert_eq!(interface_version(INTERFACE), Some(VERSION));
        assert_eq!(VERSION, 1);
        assert!(known_property_interface(INTERFACE));
        assert!(INTROSPECTION_XML.contains("<interface name=\"org.freedesktop.portal.OpenURI\">"));
    }

    #[test]
    fn an_empty_authority_is_refused() {
        for uri in ["http:///path", "https://?q", "https://#f", "https:///"] {
            assert!(parsed(&open(uri, None)).is_err(), "{uri}");
        }
        assert!(parsed(&open("https://e.example?q", None)).is_ok());
    }

    #[test]
    fn the_record_names_the_caller_and_scheme_but_never_the_link() {
        let pending = Pending {
            owner: ":1.7".into(),
            path: "/org/freedesktop/portal/desktop/request/1_7/t".into(),
            endian: Endian::Little,
            uri: "https://secret.example/token".into(),
            https: true,
            app: Some("news".into()),
            stage: Stage::Open {
                unique: ":1.9".into(),
            },
            candidates: Vec::new(),
            impostor: false,
            deadline: Instant::now(),
        };
        let line = record(&pending, 0, "opened");
        assert_eq!(
            line,
            "TD-PORTAL-OPEN-URI app=news scheme=https response=0 outcome=opened"
        );
        assert!(!line.contains("secret"));
    }
}
