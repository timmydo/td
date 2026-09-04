//! The control channel: one connection, one request, one answer.
//!
//! A keyboard and a pointer are how a PERSON drives the compositor. This is
//! how a program does — `td-ctl` is a fourth name for this artifact, and what
//! it does is write one line to a socket the compositor binds beside its
//! Wayland ones.
//!
//! Not a Wayland extension, and deliberately. A caller here has no surface, no
//! buffer and no registry: it is a one-shot that asks a question or gives one
//! order and exits, so an extension would be a protocol binding for a client
//! that does not otherwise exist. The separate socket also keeps the surface
//! out of the JAIL — td-jail names the authorities it passes through one at a
//! time and this is not among them, so a confined application cannot drive the
//! session confining it, where an extension on the public Wayland socket would
//! be reachable by everything that can open a window.
//!
//! Nothing here reaches a confined syscall. The socket is mode 0600 and the
//! kernel enforces that at `connect(2)`, so only the session's own uid can
//! open one; the private portal listener asks `SO_PEERCRED` as well, but that
//! query answers the same question its own mode already did, and widening the
//! audited caller list (§4) to ask a redundant one buys nothing. What the mode
//! does not cover is the instant between `bind` and `chmod`, and what covers
//! that is the mode-0700 runtime directory both sockets sit in.

use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::layout::{Command, Direction, Presentation, Rect, FINAL_WORKSPACE, INITIAL_WORKSPACE};
use crate::runtime::Runtime;
use crate::scene::SurfaceKey;
use crate::socket;

/// A request is one line and every verb is short, so anything approaching this
/// is a caller that has lost the protocol rather than one with a lot to say.
const REQUEST_LIMIT: usize = 1024;

/// A bound on ONE conversation — not on one `read`, which is what a socket
/// timeout gives and what an earlier draft of this relied on. `SO_RCVTIMEO`
/// restarts with every syscall, so a caller trickling one byte under it holds
/// the thread for the request limit TIMES the timeout, and the loop below is
/// serial, so that is every other caller's wait too. The deadline is what
/// makes the bound the one §15 claims. The runtime lock is never held across
/// either wait, so a slow caller cannot stop the session either.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Private to the user the compositor runs as, and the whole of the access
/// control: `connect(2)` needs write permission on the socket inode.
const CONTROL_MODE: u32 = 0o600;

/// Consecutive failed accepts before the listener gives up. The reasoning is
/// `socket::publish`'s: one failure is a caller that hung up or a moment
/// without descriptors, and retiring on it would leave a live session unable
/// to answer for the rest of its life. A RUN of them is terminal, or this
/// thread spins forever on a listener that can no longer accept.
const MAX_ACCEPT_FAILURES: u32 = 64;

/// A title is reported to say WHICH window, not to reproduce it, and a client
/// picks its own. Bounded so one cannot make a report no caller will read.
const TITLE_LIMIT: usize = 200;

/// What a caller can ask for. Every ordering variant is one `Command` the
/// keyboard already sends, so this adds a way to say them rather than a second
/// vocabulary of things to say — a control channel that could do what no key
/// can would be a second implementation of the layout to keep in step.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Request {
    /// The only question. Everything else is an order.
    Layout,
    Workspace(u8),
    Send(u8),
    Focus(Direction),
    Move(Direction),
    Fullscreen,
    Present(Presentation),
    Group,
}

/// The vocabulary, as one list, so the parser and the help text cannot drift:
/// `td-ctl help` prints this and `parse` accepts exactly it.
pub const USAGE: &[(&str, &str)] = &[
    ("layout", "report the windows, their workspaces and their rectangles"),
    ("workspace <1-9>", "show that workspace"),
    (
        "send <1-9>",
        "move the focused window to that workspace, staying where you are",
    ),
    ("focus <left|right|up|down>", "focus the window that way"),
    ("move <left|right|up|down>", "move the focused window that way"),
    ("fullscreen", "toggle fullscreen for the focused window"),
    ("present <split|stacked|tabbed>", "how its container shows its windows"),
    ("group", "group the focused window's container, or ungroup it"),
];

impl Request {
    /// Parse one request line. Errors are the caller's to read, so they name
    /// what was wrong rather than restating the whole grammar.
    pub fn parse(line: &str) -> Result<Request, String> {
        let mut words = line.split_whitespace();
        let verb = words
            .next()
            .ok_or_else(|| "empty request; try 'help'".to_string())?;
        let request = match verb {
            "layout" => Request::Layout,
            "workspace" => Request::Workspace(workspace(words.next(), verb)?),
            "send" => Request::Send(workspace(words.next(), verb)?),
            "focus" => Request::Focus(direction(words.next(), verb)?),
            "move" => Request::Move(direction(words.next(), verb)?),
            "fullscreen" => Request::Fullscreen,
            "present" => Request::Present(presentation(words.next())?),
            "group" => Request::Group,
            other => return Err(format!("no such request '{other}'; try 'help'")),
        };
        // Refused rather than ignored: a caller spelling an argument this verb
        // does not take has asked for something, and answering `ok` to a
        // request nobody made is the worst of the three answers available.
        if let Some(extra) = words.next() {
            return Err(format!("'{verb}' takes no argument '{extra}'"));
        }
        Ok(request)
    }

    /// The wire spelling, which is what the client writes. The inverse of
    /// `parse` for every variant, and its own test says so.
    pub fn render(self) -> String {
        match self {
            Request::Layout => "layout".to_string(),
            Request::Workspace(number) => format!("workspace {number}"),
            Request::Send(number) => format!("send {number}"),
            Request::Focus(direction) => format!("focus {}", direction_word(direction)),
            Request::Move(direction) => format!("move {}", direction_word(direction)),
            Request::Fullscreen => "fullscreen".to_string(),
            Request::Present(presentation) => {
                format!("present {}", presentation_word(presentation))
            }
            Request::Group => "group".to_string(),
        }
    }
}

fn workspace(word: Option<&str>, verb: &str) -> Result<u8, String> {
    let word = word.ok_or_else(|| format!("'{verb}' needs a workspace number"))?;
    let number: u8 = word
        .parse()
        .map_err(|_| format!("'{word}' is not a workspace number"))?;
    if !(INITIAL_WORKSPACE..=FINAL_WORKSPACE).contains(&number) {
        return Err(format!(
            "workspace {number} is outside {INITIAL_WORKSPACE}-{FINAL_WORKSPACE}"
        ));
    }
    Ok(number)
}

fn direction(word: Option<&str>, verb: &str) -> Result<Direction, String> {
    let word = word.ok_or_else(|| format!("'{verb}' needs a direction"))?;
    match word {
        "left" => Ok(Direction::Left),
        "right" => Ok(Direction::Right),
        "up" => Ok(Direction::Up),
        "down" => Ok(Direction::Down),
        other => Err(format!(
            "'{other}' is not a direction; try left, right, up or down"
        )),
    }
}

fn presentation(word: Option<&str>) -> Result<Presentation, String> {
    let word = word.ok_or_else(|| "'present' needs a presentation".to_string())?;
    match word {
        "split" => Ok(Presentation::Split),
        "stacked" => Ok(Presentation::Stacked),
        "tabbed" => Ok(Presentation::Tabbed),
        other => Err(format!(
            "'{other}' is not a presentation; try split, stacked or tabbed"
        )),
    }
}

fn direction_word(direction: Direction) -> &'static str {
    match direction {
        Direction::Left => "left",
        Direction::Right => "right",
        Direction::Up => "up",
        Direction::Down => "down",
    }
}

fn presentation_word(presentation: Presentation) -> &'static str {
    match presentation {
        Presentation::Split => "split",
        Presentation::Stacked => "stacked",
        Presentation::Tabbed => "tabbed",
    }
}

/// One window, as the report names it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlWindow {
    pub key: SurfaceKey,
    pub workspace: u8,
    /// Where it is laid out. Present for a window on a workspace nobody is
    /// looking at too, where it is where the window WOULD be: the arrangement
    /// of a hidden workspace is computed for the same output as the shown one,
    /// so the answer exists and refusing to give it would be a second rule.
    pub rect: Rect,
    pub visible: bool,
    pub focused: bool,
    pub fullscreen: bool,
    /// The client's own `xdg_toplevel.set_title`, or `None` from a client that
    /// has set none. Never the app id: the compositor keeps one only for the
    /// application it is watching for, so a report carrying it would be empty
    /// for every other window and look like an answer.
    pub title: Option<String>,
}

/// Everything the report says, sampled under ONE lock. One method rather than
/// an accessor per field so a report cannot describe two different instants —
/// a window list from before a switch beside the workspace number from after
/// it would be a screen that never existed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlSnapshot {
    pub width: usize,
    pub height: usize,
    pub active_workspace: u8,
    pub occupied: Vec<u8>,
    pub windows: Vec<ControlWindow>,
}

/// Render the report: one record per line, `key=value` fields in a fixed
/// order, in the house style of every other machine-readable line td writes.
///
/// `title` is LAST on its line and runs to the end of it, because a title has
/// spaces in it and a field that can eat the next one would have to be quoted;
/// last means the reader can take the rest of the line and stop. It is also
/// the one field a CLIENT chooses, so it is the one that could forge a record:
/// a title carrying a newline would otherwise print a `window` line of the
/// client's own writing. `clean_title` is what stops that.
pub fn report(snapshot: &ControlSnapshot) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "output width={} height={}\n",
        snapshot.width, snapshot.height
    ));
    let occupied: Vec<String> = snapshot
        .occupied
        .iter()
        .map(|number| number.to_string())
        .collect();
    out.push_str(&format!(
        "workspace active={} occupied={}\n",
        snapshot.active_workspace,
        occupied.join(",")
    ));
    for window in &snapshot.windows {
        out.push_str(&format!(
            "window id={}:{} workspace={} x={} y={} width={} height={} \
             visible={} focused={} fullscreen={} title={}\n",
            window.key.client,
            window.key.object,
            window.workspace,
            window.rect.x,
            window.rect.y,
            window.rect.width,
            window.rect.height,
            window.visible,
            window.focused,
            window.fullscreen,
            clean_title(window.title.as_deref()),
        ));
    }
    out
}

/// A title fit to put on a line of a record: nothing a client sets can end the
/// line early and write a record of its own, or misrepresent how the line
/// reads, and it is bounded so nothing can make a report too long to read.
///
/// Replaced rather than dropped: a title of nothing but tabs should still
/// report as a title that is there, and collapsing it to the empty string
/// would report it as a client that set none.
fn clean_title(title: Option<&str>) -> String {
    let Some(title) = title else {
        return String::new();
    };
    title
        .chars()
        .map(|character| {
            if reportable(character) {
                character
            } else {
                ' '
            }
        })
        .take(TITLE_LIMIT)
        .collect()
}

/// Whether a character may appear in a record.
///
/// `char::is_control` is Unicode category Cc and is NOT the whole answer, which
/// an earlier draft of this assumed. Two more kinds matter and neither is Cc:
///
/// - U+2028 and U+2029 END A LINE for readers that are not `str::lines` —
///   Python's `splitlines` among them, and a program in Python is exactly the
///   reader this report is for. A title carrying one forges a record there
///   while looking harmless here.
/// - the bidirectional controls and the zero-width characters change how the
///   line READS without changing what it contains. That is the same objection
///   the carriage return already answers: a person reading `td-ctl layout` out
///   of a terminal should see what the record says.
///
/// A list rather than a category test, because this crate has no Unicode
/// tables and will not grow one for a label field. Ordinary text in any script
/// survives; what does not is the set below, named with its reason.
fn reportable(character: char) -> bool {
    !character.is_control()
        && !matches!(character,
            // Line and paragraph separators (Zl, Zp).
            '\u{2028}' | '\u{2029}'
            // Zero-width and the directional marks (U+200B-200F).
            | '\u{200b}'..='\u{200f}'
            // Bidirectional embedding, override and pop (U+202A-202E).
            | '\u{202a}'..='\u{202e}'
            // Bidirectional isolates (U+2066-2069).
            | '\u{2066}'..='\u{2069}'
            // The remaining invisibles a title has no use for.
            | '\u{00ad}' | '\u{061c}' | '\u{180e}' | '\u{feff}')
}

/// Apply one request and answer it. The report is BUILT here, under the
/// caller's lock, and written after it is released.
pub fn apply(runtime: &mut Runtime, request: Request) -> Result<Option<String>, String> {
    let command = match request {
        Request::Layout => return Ok(Some(report(&runtime.control_snapshot()))),
        Request::Workspace(number) => Command::SwitchWorkspace(number),
        Request::Send(number) => Command::MoveToWorkspace(number),
        Request::Focus(direction) => Command::Focus(direction),
        Request::Move(direction) => Command::Move(direction),
        Request::Fullscreen => Command::ToggleFullscreen,
        Request::Present(presentation) => Command::SetPresentation(presentation),
        Request::Group => Command::ToggleGrouped,
    };
    // `Runtime::command`, the keyboard's own path, whole-output damage and
    // all: a scripted tiling command is the same gesture as the typed one and
    // must not repair the screen a different amount.
    runtime.command(command)?;
    Ok(None)
}

/// The answer to one request: a status line, then a body only a question has.
///
/// THREE statuses, not two. `error` is the request's fault and `unavailable`
/// is the compositor's, and a caller wants them apart for the same reason the
/// client's two exit codes are apart: one says fix the command and the other
/// says fix the session. A poisoned runtime and a paint that failed are the
/// compositor's, and answering them as refusals told a script to go and check
/// its spelling.
pub fn answer(runtime: &Mutex<Runtime>, line: &str) -> String {
    let request = match Request::parse(line) {
        Ok(request) => request,
        Err(error) => return format!("error {error}\n"),
    };
    // A poisoned runtime is a compositor that has already lost; say so rather
    // than take this thread down after it.
    let outcome = match runtime.lock() {
        Ok(mut runtime) => apply(&mut runtime, request),
        Err(_) => Err("compositor runtime is poisoned".to_string()),
    };
    match outcome {
        Ok(None) => "ok\n".to_string(),
        Ok(Some(report)) => format!("ok\n{report}"),
        Err(error) => format!("unavailable {error}\n"),
    }
}

/// Bind the control socket and answer it on a thread of its own.
///
/// A failure here is FATAL to the caller, which an earlier draft had the other
/// way round on the status bar's reasoning. The bar is a decoration; this is
/// an endpoint whose name the session hands to every program it starts.
/// `socket::remove_stale` already clears a socket nobody answers, so the only
/// way past it is a path something LIVE owns — and coming up anyway would
/// advertise that path through `TD_CONTROL_SOCKET` while the incumbent
/// answered on it, handing a caller someone else's report at exit 0.
pub fn serve(path: &Path, runtime: Arc<Mutex<Runtime>>) -> Result<(), String> {
    socket::remove_stale(path, "control")?;
    let listener = UnixListener::bind(path)
        .map_err(|error| format!("bind control socket {}: {error}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(CONTROL_MODE))
        .map_err(|error| format!("chmod control socket {}: {error}", path.display()))?;
    // `thread::Builder`, not `thread::spawn`: the latter panics when the OS
    // refuses a thread, and this crate does not panic.
    thread::Builder::new()
        .name("td-control".to_string())
        .spawn(move || accept(listener.incoming(), &runtime))
        .map(|_| ())
        .map_err(|error| format!("start control listener {}: {error}", path.display()))
}

/// Takes the connections rather than the listener, for `socket::serve`'s
/// reason: a bound on a run of failed accepts that no test can reach is a
/// bound nobody can trust.
fn accept(connections: impl Iterator<Item = io::Result<UnixStream>>, runtime: &Mutex<Runtime>) {
    let mut consecutive = 0;
    for connection in connections {
        let Ok(stream) = connection else {
            consecutive += 1;
            if consecutive > MAX_ACCEPT_FAILURES {
                eprintln!("td-compositor: control: too many failed accepts, listener retired");
                return;
            }
            continue;
        };
        consecutive = 0;
        // Serially, one caller at a time. A control request is a line and an
        // answer, every wait it can do is bounded, and the alternative is a
        // thread per caller whose only purpose would be to let two callers
        // reorder each other's orders.
        if let Err(error) = converse(stream, runtime) {
            eprintln!("td-compositor: control: {error}");
        }
    }
}

fn converse(mut stream: UnixStream, runtime: &Mutex<Runtime>) -> Result<(), String> {
    // One deadline for the whole exchange. The socket timeouts stay, because
    // they are what unblocks a single stalled syscall; the deadline is what
    // bounds a caller whose every syscall returns promptly and slowly.
    let deadline = Instant::now()
        .checked_add(IO_TIMEOUT)
        .ok_or_else(|| "control deadline is beyond the clock".to_string())?;
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .map_err(|error| format!("set control read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(IO_TIMEOUT))
        .map_err(|error| format!("set control write timeout: {error}"))?;
    // A request that could not be READ is answered too, not just dropped: a
    // caller whose line was too long otherwise sees an empty answer and has to
    // guess between "the compositor refused me" and "there was no compositor".
    // Its fault, so `error` — the limit and the encoding are both the caller's
    // to get right — and the write below is bounded by the same deadline, so
    // answering a caller still mid-flood cannot park this thread.
    let reply = match read_request(&mut stream, deadline) {
        Ok(line) => answer(runtime, &line),
        Err(error) => format!("error {error}\n"),
    };
    write_answer(&mut stream, reply.as_bytes(), deadline)
}

/// Read one line, bounded in BYTES by `REQUEST_LIMIT` and in TIME by
/// `deadline`. Ends at the first newline or at end of input, so a caller that
/// writes its request and shuts down is answered exactly as one that
/// terminates its request.
fn read_request(stream: &mut UnixStream, deadline: Instant) -> Result<String, String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 256];
    loop {
        if Instant::now() >= deadline {
            return Err("control request timed out".to_string());
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                let Some(read) = chunk.get(..count) else {
                    break;
                };
                let (line, done) = match read.iter().position(|byte| *byte == b'\n') {
                    Some(end) => (read.get(..end).unwrap_or_default(), true),
                    None => (read, false),
                };
                buffer.extend_from_slice(line);
                if buffer.len() > REQUEST_LIMIT {
                    return Err(format!("control request exceeds {REQUEST_LIMIT} bytes"));
                }
                if done {
                    break;
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("read control request: {error}")),
        }
    }
    String::from_utf8(buffer).map_err(|_| "control request is not UTF-8".to_string())
}

/// Write the answer under the same deadline. Not `write_all`, which loops over
/// partial writes with no bound of its own: a caller reading one byte at a
/// time would keep every single write fast and the whole answer endless.
fn write_answer(stream: &mut UnixStream, answer: &[u8], deadline: Instant) -> Result<(), String> {
    let mut written = 0;
    while written < answer.len() {
        if Instant::now() >= deadline {
            return Err("control answer timed out".to_string());
        }
        let Some(rest) = answer.get(written..) else {
            break;
        };
        match stream.write(rest) {
            Ok(0) => return Err("control answer went nowhere".to_string()),
            Ok(count) => written = written.saturating_add(count),
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("write control answer: {error}")),
        }
    }
    Ok(())
}

/// The client half: `td-ctl`. Connect, say one thing, print what comes back.
///
/// The exit status is the answer's status line, so a script can branch on it
/// without reading the text — which is the whole reason the status is a line
/// of its own rather than a word in front of the report.
pub fn ask(path: &Path, request: Request) -> Result<String, ControlFailure> {
    let mut stream = UnixStream::connect(path).map_err(|error| {
        ControlFailure::Unreachable(format!(
            "connect control socket {}: {error}",
            path.display()
        ))
    })?;
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(IO_TIMEOUT)))
        .map_err(|error| ControlFailure::Unreachable(format!("set control timeout: {error}")))?;
    let mut line = request.render();
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(|error| ControlFailure::Unreachable(format!("write control request: {error}")))?;
    let answer = read_answer(&mut stream)?;
    let answer = String::from_utf8(answer).map_err(|_| {
        ControlFailure::Unreachable("control answer is not UTF-8".to_string())
    })?;
    split_answer(&answer)
}

/// Read to the end, but KEEP what arrived before a failure.
///
/// The compositor answers an over-long request and then closes with the
/// caller's unread bytes still queued, which reaches the caller as
/// `ConnectionReset` — and discarding the answer on that error threw away the
/// very refusal the compositor wrote to spare the caller a guess. Its own
/// function because that rule is not reachable through `ask`: `ask` renders
/// the request itself, so no request it can send provokes the close.
fn read_answer(stream: &mut UnixStream) -> Result<Vec<u8>, ControlFailure> {
    let mut answer = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => match chunk.get(..count) {
                Some(read) => answer.extend_from_slice(read),
                None => break,
            },
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => {
                if answer.is_empty() {
                    return Err(ControlFailure::Unreachable(format!(
                        "read control answer: {error}"
                    )));
                }
                break;
            }
        }
    }
    Ok(answer)
}

/// What the client makes of an answer. The compositor refusing a request is
/// not the same failure as never reaching one, and a caller that cannot tell
/// them apart cannot tell a typo from a session that is not running.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlFailure {
    Unreachable(String),
    Refused(String),
}

impl ControlFailure {
    pub fn message(&self) -> &str {
        match self {
            ControlFailure::Unreachable(message) | ControlFailure::Refused(message) => message,
        }
    }

    /// Two statuses rather than one, for the reason above: 2 is "the
    /// compositor said no", 1 is "there was nobody to ask".
    pub fn exit_code(&self) -> i32 {
        match self {
            ControlFailure::Unreachable(_) => 1,
            ControlFailure::Refused(_) => 2,
        }
    }
}

/// Split a status line from its body.
///
/// The terminator is REQUIRED. `splitn` would hand back the whole answer as a
/// status when there is no newline in it, so a truncated `ok` — a compositor
/// killed mid-write, or something else entirely on the path — would exit 0 as
/// a success carrying no report. Nothing on this protocol writes a status
/// without a newline, so demanding one costs a caller nothing and turns a
/// half-answer into the failure it is.
fn split_answer(answer: &str) -> Result<String, ControlFailure> {
    let Some((status, body)) = answer.split_once('\n') else {
        return Err(ControlFailure::Unreachable(format!(
            "control answer has no status line: '{answer}'"
        )));
    };
    if status == "ok" {
        // Every record the report writes ends in a newline, so a body that
        // does not is one the compositor gave up part way through — which
        // either the deadline or the socket's write timeout can cause. Same
        // argument as the status line's own terminator, one level down. A
        // truncation landing exactly on a record boundary still reads as a
        // shorter report; bounding that needs a length the protocol does not
        // carry.
        if !body.is_empty() && !body.ends_with('\n') {
            return Err(ControlFailure::Unreachable(
                "control answer body is truncated".to_string(),
            ));
        }
        return Ok(body.to_string());
    }
    if let Some(message) = status.strip_prefix("error ") {
        return Err(ControlFailure::Refused(message.to_string()));
    }
    // The compositor's own fault rather than the request's, so it is the
    // exit code that says "there is nothing to ask" and not the one that says
    // "you asked wrongly".
    if let Some(message) = status.strip_prefix("unavailable ") {
        return Err(ControlFailure::Unreachable(message.to_string()));
    }
    Err(ControlFailure::Unreachable(format!(
        "control answer is not a status line: '{status}'"
    )))
}

/// The help text, which is `USAGE` and nothing else — so a request the parser
/// takes and the help does not name is impossible to write.
pub fn help() -> String {
    let mut out = String::from("td-ctl [--socket PATH] <request>\n\n");
    for (form, what) in USAGE {
        out.push_str(&format!("  {form:<30} {what}\n"));
    }
    // Where the socket comes from, said HERE, because a caller who has to
    // discover it by failing first has been told nothing by the failure that
    // the help could not have told it in advance. `--socket` comes first or
    // not at all: everything after the request's own words is the request.
    out.push_str(
        "\nThe socket is $TD_CONTROL_SOCKET unless --socket names one, and\n\
         --socket must come before the request.\n\
         Exit: 0 answered, 2 the request was refused, 1 no compositor answered.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_request_survives_the_round_trip_its_own_spelling_makes() {
        // `render` is what the client writes and `parse` is what the server
        // reads, so a variant they disagree about is one the binary cannot
        // send. Every variant, since the pair is written by hand.
        let every = [
            Request::Layout,
            Request::Workspace(1),
            Request::Workspace(9),
            Request::Send(4),
            Request::Focus(Direction::Left),
            Request::Focus(Direction::Right),
            Request::Focus(Direction::Up),
            Request::Focus(Direction::Down),
            Request::Move(Direction::Left),
            Request::Move(Direction::Down),
            Request::Fullscreen,
            Request::Present(Presentation::Split),
            Request::Present(Presentation::Stacked),
            Request::Present(Presentation::Tabbed),
            Request::Group,
        ];
        for request in every {
            let line = request.render();
            assert_eq!(
                Request::parse(&line),
                Ok(request),
                "'{line}' did not parse back to what wrote it"
            );
        }
    }

    #[test]
    fn the_help_text_names_every_verb_the_parser_takes() {
        // The two lists are one list, and this is what says so: a verb added
        // to the parser and not to `USAGE` is a request nobody can discover.
        let help = help();
        for verb in [
            "layout",
            "workspace",
            "send",
            "focus",
            "move",
            "fullscreen",
            "present",
            "group",
        ] {
            assert!(help.contains(verb), "help text does not name '{verb}'");
        }
        // And the other way: every USAGE entry's first word parses, given the
        // argument its own form names.
        for (form, _) in USAGE {
            let verb = form.split_whitespace().next().unwrap();
            let line = match verb {
                "workspace" | "send" => format!("{verb} 1"),
                "focus" | "move" => format!("{verb} left"),
                "present" => format!("{verb} split"),
                other => other.to_string(),
            };
            assert!(
                Request::parse(&line).is_ok(),
                "help names '{form}' but '{line}' does not parse"
            );
        }
    }

    #[test]
    fn a_request_that_is_not_one_is_refused_by_name() {
        for (line, expected) in [
            ("", "empty request"),
            ("   ", "empty request"),
            ("dance", "no such request 'dance'"),
            ("workspace", "'workspace' needs a workspace number"),
            ("workspace x", "'x' is not a workspace number"),
            ("workspace 0", "workspace 0 is outside 1-9"),
            ("workspace 10", "workspace 10 is outside 1-9"),
            ("send 0", "workspace 0 is outside 1-9"),
            ("focus", "'focus' needs a direction"),
            ("focus sideways", "'sideways' is not a direction"),
            ("present", "'present' needs a presentation"),
            ("present flat", "'flat' is not a presentation"),
            // An argument to a verb that takes none is a caller asking for
            // something else, and answering `ok` would be answering a request
            // nobody made.
            ("layout now", "'layout' takes no argument 'now'"),
            ("fullscreen 1", "'fullscreen' takes no argument '1'"),
            ("workspace 1 2", "'workspace' takes no argument '2'"),
        ] {
            let error = Request::parse(line).expect_err("'{line}' was accepted");
            assert!(
                error.contains(expected),
                "'{line}' was refused as '{error}', which does not name '{expected}'"
            );
        }
        // 256 is not a u8 and 1e0 is not an integer; both are refused as what
        // they are rather than wrapping into a workspace that exists.
        assert!(Request::parse("workspace 256").is_err());
        assert!(Request::parse("workspace 1e0").is_err());
    }

    #[test]
    fn a_clients_title_cannot_write_a_record_of_its_own() {
        // The one field a client chooses. A newline in it would end the line
        // and leave the rest looking like a record the compositor wrote — the
        // report's only injection, and this is the answer to it.
        let snapshot = ControlSnapshot {
            width: 100,
            height: 50,
            active_workspace: 1,
            occupied: vec![1],
            windows: vec![ControlWindow {
                key: SurfaceKey {
                    client: 7,
                    object: 3,
                },
                workspace: 1,
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 50,
                },
                visible: true,
                focused: true,
                fullscreen: false,
                title: Some("evil\nwindow id=9:9 workspace=2".to_string()),
            }],
        };
        let report = report(&snapshot);
        assert_eq!(
            report.lines().count(),
            3,
            "the title wrote a line of its own:\n{report}"
        );
        // The text itself survives, and must: a title is allowed to SAY
        // anything. What it may not do is become a record, and one `window`
        // line for one window is what says it did not.
        assert_eq!(
            report
                .lines()
                .filter(|line| line.starts_with("window "))
                .count(),
            1,
            "the forged record became a line of its own:\n{report}"
        );
        assert!(report.contains("title=evil window id=9:9 workspace=2"));
        // Every control character, not only the newline: a carriage return
        // repaints a line on a terminal, which hides what came before it.
        assert_eq!(clean_title(Some("a\r\t\u{7f}b")), "a   b");
        // A title of nothing but control characters is still a title that is
        // there, which is not the same answer as a client that set none.
        assert_eq!(clean_title(Some("\n")), " ");
        assert_eq!(clean_title(None), "");
        let long: String = std::iter::repeat_n('x', TITLE_LIMIT + 50).collect();
        assert_eq!(clean_title(Some(&long)).chars().count(), TITLE_LIMIT);
    }

    #[test]
    fn the_report_names_every_window_and_where_it_is() {
        let snapshot = ControlSnapshot {
            width: 800,
            height: 600,
            active_workspace: 2,
            occupied: vec![1, 2],
            windows: vec![
                ControlWindow {
                    key: SurfaceKey {
                        client: 1,
                        object: 5,
                    },
                    workspace: 1,
                    rect: Rect {
                        x: 0,
                        y: 24,
                        width: 800,
                        height: 576,
                    },
                    visible: false,
                    focused: false,
                    fullscreen: false,
                    title: Some("shell".to_string()),
                },
                ControlWindow {
                    key: SurfaceKey {
                        client: 2,
                        object: 9,
                    },
                    workspace: 2,
                    rect: Rect {
                        x: 0,
                        y: 24,
                        width: 400,
                        height: 576,
                    },
                    visible: true,
                    focused: true,
                    fullscreen: false,
                    title: None,
                },
            ],
        };
        assert_eq!(
            report(&snapshot),
            "output width=800 height=600\n\
             workspace active=2 occupied=1,2\n\
             window id=1:5 workspace=1 x=0 y=24 width=800 height=576 \
             visible=false focused=false fullscreen=false title=shell\n\
             window id=2:9 workspace=2 x=0 y=24 width=400 height=576 \
             visible=true focused=true fullscreen=false title=\n"
        );
    }

    /// A framebuffer and a runtime behind a lock, which is what the compositor
    /// hands the control thread.
    struct Cleanup(std::path::PathBuf);

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn temporary(what: &str) -> Cleanup {
        // Short, because a Unix socket path is bounded well below a path.
        Cleanup(std::env::temp_dir().join(format!(
            "td-{what}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )))
    }

    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    fn session(window: SurfaceKey) -> (Arc<Mutex<Runtime>>, Cleanup) {
        let frame = temporary("control-fb");
        let framebuffer = crate::framebuffer::Framebuffer::test_file(&frame.0, 240, 600, 240 * 4)
            .expect("test framebuffer");
        let mut runtime = Runtime::new(framebuffer);
        runtime
            .commit(
                window,
                crate::buffer::Surface::from_shm_pixels(
                    100,
                    100,
                    [1u8, 2, 3, 0].repeat(10_000),
                    crate::scene::SHM_XRGB8888,
                )
                .expect("test surface"),
            )
            .expect("commit");
        (Arc::new(Mutex::new(runtime)), frame)
    }

    #[test]
    fn a_request_crosses_a_real_socket_and_moves_the_session() {
        // The whole path, through the kernel: the bind, the accept, the line,
        // the parse, the command, and the answer. Every unit below this is a
        // piece of it, and none of them proves the pieces are joined up.
        let window = SurfaceKey {
            client: 1,
            object: 1,
        };
        let (runtime, _frame) = session(window);
        let socket = temporary("control-sock");
        serve(&socket.0, Arc::clone(&runtime)).expect("serve");

        // THE QUESTION. One window, on workspace 1, which is where a session
        // starts and what the strip would be showing.
        let report = ask(&socket.0, Request::Layout).expect("layout");
        assert!(
            report.contains("workspace active=1 occupied=1\n"),
            "layout did not report the workspace in view:\n{report}"
        );
        assert!(
            report.contains("window id=1:1 workspace=1 "),
            "layout did not report the window:\n{report}"
        );
        assert!(report.contains("focused=true"), "{report}");

        // AN ORDER, and the session moved: read back through the same socket
        // rather than out of the runtime, since what a caller can SEE is the
        // whole of what this surface promises.
        assert_eq!(ask(&socket.0, Request::Workspace(2)), Ok(String::new()));
        let report = ask(&socket.0, Request::Layout).expect("layout");
        assert!(
            report.contains("workspace active=2 occupied=1\n"),
            "the switch did not take:\n{report}"
        );
        // The WINDOW stayed where it was — a switch shows a workspace, it does
        // not carry anything to it.
        assert!(report.contains("window id=1:1 workspace=1 "), "{report}");
        assert!(
            report.contains("visible=false"),
            "a window on another workspace is not on screen:\n{report}"
        );
        assert_eq!(
            runtime.lock().expect("runtime").control_snapshot().active_workspace,
            2
        );

        // And the accept loop OUTLIVED both, which one connection cannot say.
        assert_eq!(ask(&socket.0, Request::Workspace(1)), Ok(String::new()));
        assert_eq!(ask(&socket.0, Request::Send(3)), Ok(String::new()));
        let report = ask(&socket.0, Request::Layout).expect("layout");
        assert!(
            report.contains("window id=1:1 workspace=3 "),
            "the window did not go to workspace 3:\n{report}"
        );
        // The view did NOT follow it, which is what `send` means here and what
        // the keyboard's own `MoveToWorkspace` has always done: the window
        // goes, the operator stays. Worth pinning rather than assuming — it is
        // the one answer in this vocabulary a caller could reasonably expect
        // the other way round, and the help text says which it is.
        assert!(
            report.contains("workspace active=1 "),
            "sending a window carried the view with it:\n{report}"
        );
        assert!(
            report.contains("window id=1:1 workspace=3 ") && report.contains("visible=false"),
            "the window did not go on without the view:\n{report}"
        );

        // FULLSCREEN is the arrangement's fact, not the renderer's. Made
        // fullscreen where it is, then looked at from another workspace: the
        // renderer's own flag is false for a window nobody is looking at, so
        // reporting that one would say `fullscreen=false` for a window whose
        // rectangle covers the whole output and leave the overlap it makes
        // with its neighbours unexplained.
        assert_eq!(ask(&socket.0, Request::Workspace(3)), Ok(String::new()));
        assert_eq!(ask(&socket.0, Request::Fullscreen), Ok(String::new()));
        let report = ask(&socket.0, Request::Layout).expect("layout");
        assert!(
            report.contains("visible=true focused=true fullscreen=true"),
            "the window did not go fullscreen where it is:\n{report}"
        );
        assert_eq!(ask(&socket.0, Request::Workspace(1)), Ok(String::new()));
        let report = ask(&socket.0, Request::Layout).expect("layout");
        assert!(
            report.contains("fullscreen=true"),
            "a hidden fullscreen window reported as an ordinary tile:\n{report}"
        );
        assert!(report.contains("visible=false"), "{report}");

        // A REFUSAL is answered and the socket stays up, which is the shape a
        // caller with a typo leaves behind.
        assert_eq!(
            ask(&socket.0, Request::Workspace(1)),
            Ok(String::new()),
            "the socket stopped answering after a refusal"
        );
    }

    #[test]
    fn the_control_socket_is_private_to_the_session() {
        // The whole of the access control, so it is the whole of what this
        // proves: `connect(2)` wants write permission on the inode, and 0600
        // is what says only this uid has it.
        let window = SurfaceKey {
            client: 1,
            object: 1,
        };
        let (runtime, _frame) = session(window);
        let socket = temporary("control-mode");
        serve(&socket.0, runtime).expect("serve");
        let mode = std::fs::metadata(&socket.0)
            .expect("stat control socket")
            .permissions()
            .mode()
            & 0o777;
        // The literal §15 specifies, not the constant under test: asserting
        // `CONTROL_MODE` passes for whatever `CONTROL_MODE` happens to say, so
        // a socket changed to 0666 would ship with the gate green. `socket.rs`
        // pins its own mode this way for the same reason.
        assert_eq!(mode, 0o600, "the control socket is not private");
        assert_eq!(CONTROL_MODE, 0o600);
    }

    #[test]
    fn a_refusal_crosses_the_socket_as_a_refusal() {
        // The status line is what a script branches on, so the two answers
        // have to survive the wire differently — and the compositor must not
        // act on a request it refused.
        let window = SurfaceKey {
            client: 1,
            object: 1,
        };
        let (runtime, _frame) = session(window);
        let socket = temporary("control-refuse");
        serve(&socket.0, Arc::clone(&runtime)).expect("serve");
        // Straight onto the wire, since `Request` cannot spell a bad request:
        // this is the shape another program writing the protocol would send.
        let mut stream = UnixStream::connect(&socket.0).expect("connect");
        stream.write_all(b"dance\n").expect("write");
        let mut answer = String::new();
        stream.read_to_string(&mut answer).expect("read");
        assert_eq!(answer, "error no such request 'dance'; try 'help'\n");
        assert_eq!(
            runtime.lock().expect("runtime").control_snapshot().active_workspace,
            1,
            "a refused request moved the session anyway"
        );
        // A request with no newline at all is still one request: a caller that
        // writes and shuts down is answered like one that terminates its line.
        let mut stream = UnixStream::connect(&socket.0).expect("connect");
        stream.write_all(b"workspace 2").expect("write");
        stream.shutdown(std::net::Shutdown::Write).expect("shutdown");
        let mut answer = String::new();
        stream.read_to_string(&mut answer).expect("read");
        assert_eq!(answer, "ok\n");
        assert_eq!(
            runtime.lock().expect("runtime").control_snapshot().active_workspace,
            2
        );
    }

    #[test]
    fn a_request_longer_than_the_limit_is_refused_rather_than_read() {
        let window = SurfaceKey {
            client: 1,
            object: 1,
        };
        let (runtime, _frame) = session(window);
        let socket = temporary("control-flood");
        serve(&socket.0, runtime).expect("serve");
        let mut stream = UnixStream::connect(&socket.0).expect("connect");
        // No newline, so nothing bounds this but the limit itself.
        let flood = vec![b'x'; REQUEST_LIMIT * 4];
        // The write may fail once the far end gives up and closes, which is
        // the refusal arriving rather than a problem: what matters is that the
        // compositor stopped reading instead of buffering whatever came.
        let _ = stream.write_all(&flood);
        let mut answer = String::new();
        let _ = stream.read_to_string(&mut answer);
        // Answered, and answered as a refusal: the caller learns the limit
        // rather than being left with an empty socket to interpret. Strictly,
        // with no `is_empty()` escape — the escape made this test pass with
        // the answer deleted altogether, which is the whole of what it is for.
        assert_eq!(
            answer, "error control request exceeds 1024 bytes\n",
            "a flood was not answered with the limit it broke"
        );
    }

    #[test]
    fn a_caller_that_trickles_cannot_hold_the_socket() {
        // The bound is on the CONVERSATION, not on one `read`. A socket
        // timeout restarts with every syscall, so a caller sending one byte
        // just inside it held this thread for the request limit times the
        // timeout — and the loop is serial, so that was every other caller's
        // wait too.
        //
        // Asked of `read_request` directly and with a deadline already spent,
        // rather than by trickling at a live socket and timing it: the
        // property is that the deadline ENDS the read, and a test that sleeps
        // its way to five seconds proves the same thing slower and flakier.
        let (mut mine, theirs) = UnixStream::pair().expect("pair");
        let mut theirs = theirs;
        // A partial request: bytes, but no newline, so nothing but the
        // deadline can end this read.
        mine.write_all(b"workspace 2").expect("write");
        let spent = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("a clock before the epoch of this process");
        let refused = read_request(&mut theirs, spent).expect_err("a spent deadline was read past");
        assert!(
            refused.contains("timed out"),
            "the deadline refused for the wrong reason: {refused}"
        );
        // And with time left, the same partial request is still waited for —
        // so what ended it above was the deadline and not the partial line.
        theirs
            .set_read_timeout(Some(Duration::from_millis(200)))
            .expect("timeout");
        let live = Instant::now()
            .checked_add(Duration::from_secs(30))
            .expect("clock");
        let waited = read_request(&mut theirs, live).expect_err("a partial line ended a request");
        assert!(
            !waited.contains("timed out"),
            "the read gave up on its own deadline rather than the socket's"
        );
    }

    #[test]
    fn the_answer_is_written_under_the_deadline_too() {
        // The write half of the same bound. `write_all` loops over partial
        // writes with no bound of its own, so a caller reading one byte at a
        // time keeps every write fast and the whole answer endless — and with
        // the accept loop serial that is every other caller's wait. Asked of
        // `write_answer` with the deadline already spent, for the reason the
        // read half is asked that way: the property is that the deadline ends
        // the write, not that a slow reader can be timed.
        let (_mine, mut theirs) = UnixStream::pair().expect("pair");
        let spent = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("a clock before the epoch of this process");
        let refused = write_answer(&mut theirs, b"ok\n", spent)
            .expect_err("a spent deadline was written past");
        assert!(
            refused.contains("timed out"),
            "the deadline refused for the wrong reason: {refused}"
        );
        // And with time left the same answer goes out, so what stopped it
        // above was the deadline and not the socket.
        let live = Instant::now()
            .checked_add(Duration::from_secs(30))
            .expect("clock");
        write_answer(&mut theirs, b"ok\n", live).expect("a live deadline refused a write");
    }

    #[test]
    fn an_answer_that_arrived_survives_the_error_that_followed_it() {
        // The compositor answers an over-long request and then closes with
        // the caller's unread bytes queued, which reaches the caller as an
        // error AFTER the refusal is already in hand. Discarding it there
        // turned an exit-2 refusal into exit 1 "there was no compositor" —
        // the very ambiguity the answer exists to prevent.
        //
        // Asked of `read_answer` directly: `ask` renders its own request, so
        // no request it can send provokes the close. A short read timeout
        // with the peer still open produces the same shape — bytes, then an
        // error — without a race to lose.
        let (mut mine, theirs) = UnixStream::pair().expect("pair");
        let mut theirs = theirs;
        theirs
            .set_read_timeout(Some(Duration::from_millis(50)))
            .expect("timeout");
        mine.write_all(b"error control request exceeds 1024 bytes\n")
            .expect("write");
        let answer = read_answer(&mut theirs).expect("the answer was thrown away");
        assert_eq!(
            String::from_utf8(answer).expect("utf8"),
            "error control request exceeds 1024 bytes\n",
            "the read kept none of what had already arrived"
        );
        // And with nothing received, the error is still the answer.
        let (_empty, mut alone) = UnixStream::pair().expect("pair");
        alone
            .set_read_timeout(Some(Duration::from_millis(50)))
            .expect("timeout");
        read_answer(&mut alone).expect_err("an empty read reported success");
    }

    #[test]
    fn a_truncated_report_is_not_a_short_one() {
        // A write can give up part way through a body — the deadline, or the
        // socket's own write timeout. Every record ends in a newline, so a
        // body that does not is one that stopped early, and reading it as a
        // complete, shorter report is exit 0 with windows missing.
        assert!(matches!(
            split_answer("ok\noutput width=1 height=2\nwindow id=1:1 works"),
            Err(ControlFailure::Unreachable(_))
        ));
        assert_eq!(
            split_answer("ok\noutput width=1 height=2\n"),
            Ok("output width=1 height=2\n".to_string())
        );
    }

    #[test]
    fn a_run_of_failed_accepts_is_survived_but_not_an_endless_one() {
        // `accept` takes the connections rather than the listener so that this
        // bound is reachable at all. One failure is a caller that hung up or a
        // moment without descriptors; retiring on it would leave a live
        // session unable to answer for the rest of its life.
        let window = SurfaceKey {
            client: 1,
            object: 1,
        };
        let (runtime, _frame) = session(window);
        let refuse = || Err(io::Error::from(ErrorKind::ConnectionAborted));
        // A caller that hung up mid-answer closes with bytes unread, which
        // reaches this side as a reset rather than an end. Either is "no
        // answer" for what these assertions are about.
        let answer_of = |stream: &mut UnixStream| {
            let mut answer = String::new();
            let _ = stream.read_to_string(&mut answer);
            answer
        };

        // A run just under the bound, then a real caller: survived.
        let (mine, theirs) = UnixStream::pair().expect("pair");
        let mut mine = mine;
        mine.write_all(b"workspace 2\n").expect("write");
        let connections = std::iter::repeat_with(refuse)
            .take(MAX_ACCEPT_FAILURES as usize)
            .chain(std::iter::once(Ok(theirs)));
        accept(connections, &runtime);
        assert_eq!(
            answer_of(&mut mine),
            "ok\n",
            "a run of failed accepts buried the caller after it"
        );
        assert_eq!(
            runtime.lock().expect("runtime").control_snapshot().active_workspace,
            2
        );

        // One more failure than the bound, and the listener retires rather
        // than spinning on a socket that can no longer accept anything.
        let (mine, theirs) = UnixStream::pair().expect("pair");
        let mut mine = mine;
        mine.write_all(b"workspace 1\n").expect("write");
        let connections = std::iter::repeat_with(refuse)
            .take((MAX_ACCEPT_FAILURES as usize).saturating_add(1))
            .chain(std::iter::once(Ok(theirs)));
        accept(connections, &runtime);
        assert_eq!(
            answer_of(&mut mine),
            "",
            "the listener answered past its own bound"
        );
        assert_eq!(
            runtime.lock().expect("runtime").control_snapshot().active_workspace,
            2,
            "a caller past the bound was served anyway"
        );
    }

    #[test]
    fn a_title_cannot_forge_a_record_for_a_reader_that_is_not_rust() {
        // `char::is_control` is category Cc and stops at it. These are not Cc
        // and every one of them either ends a line for some reader or changes
        // how the line reads for a person.
        for (character, what) in [
            ('\u{2028}', "line separator"),
            ('\u{2029}', "paragraph separator"),
            ('\u{202e}', "right-to-left override"),
            ('\u{2066}', "left-to-right isolate"),
            ('\u{200b}', "zero width space"),
            ('\u{feff}', "zero width no-break space"),
            ('\u{00ad}', "soft hyphen"),
        ] {
            assert!(
                !reportable(character),
                "{what} ({character:?}) survives into a record"
            );
            assert_eq!(clean_title(Some(&format!("a{character}b"))), "a b");
        }
        // And ordinary text in any script is NOT collapsed: this is a filter
        // on a named set, not an ASCII allowlist, because a window titled in
        // Japanese should still be identifiable in the report.
        for text in ["café", "日本語", "Ελληνικά", "x"] {
            assert_eq!(clean_title(Some(text)), text);
        }
    }

    #[test]
    fn the_compositors_own_failure_is_not_the_callers_fault() {
        // Two statuses for two different things to fix. A refusal says the
        // request was wrong; `unavailable` says the compositor could not, and
        // sending a script to check its spelling for a poisoned runtime is the
        // wrong instruction.
        assert_eq!(
            split_answer("unavailable compositor runtime is poisoned\n"),
            Err(ControlFailure::Unreachable(
                "compositor runtime is poisoned".to_string()
            ))
        );
        assert_eq!(
            ControlFailure::Unreachable(String::new()).exit_code(),
            1,
            "the compositor's own failure exits as a bad request"
        );
        // A status line with no newline is not a status line. A compositor
        // killed mid-write would otherwise exit 0 with no report at all.
        assert!(matches!(
            split_answer("ok"),
            Err(ControlFailure::Unreachable(_))
        ));
        assert!(matches!(
            split_answer("error truncated"),
            Err(ControlFailure::Unreachable(_))
        ));

        // And the SERVER's half of the same claim, which the client's cannot
        // stand in for: answering the compositor's own failure as `error`
        // passed every assertion above. A poisoned runtime is the reachable
        // one of the two failures `answer` reports.
        let (runtime, _frame) = session(SurfaceKey {
            client: 7,
            object: 7,
        });
        let poisoned = Arc::clone(&runtime);
        let _ = thread::spawn(move || {
            let _held = poisoned.lock().expect("lock to poison");
            panic!("poisoning the runtime on purpose");
        })
        .join();
        assert!(runtime.is_poisoned(), "the runtime did not poison");
        let answered = answer(&runtime, "layout");
        assert!(
            answered.starts_with("unavailable "),
            "the compositor's own failure crossed as the caller's fault: {answered}"
        );
    }

    #[test]
    fn the_help_text_is_the_only_place_a_verb_can_hide() {
        // `USAGE` feeds `help()`, and this reads the PARSER's own arms out of
        // the source to walk the link the other way. Without it a verb added
        // to the match and not to `USAGE` ships undocumented, which is exactly
        // what a mutation demonstrated.
        //
        // What it reads is the ARMS of `match verb`. Dispatch moved out of
        // them is outside what it can see — a helper called from the
        // catch-all, a nested match inside an arm's body, a check before the
        // match — and each of those hides a verb from it. Reading the arms
        // is what makes it a source scan rather than a parser; closing that
        // class would mean parsing Rust here, and the answer to a verb
        // deliberately hidden from its own documentation is review, not a
        // better scanner.
        let source = include_str!("control.rs");
        let (parse, _) = source
            .split_once("        // Refused rather than ignored")
            .expect("the parser's match no longer ends where this looks");
        let (_, arms) = parse
            .split_once("let request = match verb {")
            .expect("the parser's match no longer begins where this looks");
        let mut found = 0;
        for line in arms.lines() {
            let line = line.trim();
            if !line.starts_with('"') {
                // The catch-all is the one arm that names no verb. Anything
                // else without a leading literal is an arm this scanner
                // cannot read — a guard arm, `other if other == "windows"`,
                // took a verb straight past the quoted-line filter and never
                // reached the arrow check below.
                assert!(
                    !line.contains("=>") || line.starts_with("other =>"),
                    "this scanner reads the arms of `match verb` one per line, \
                     each leading with a quoted verb except the catch-all, \
                     which it expects spelled `other =>`. It cannot read: \
                     {line}"
                );
                continue;
            }
            // Every literal in the arm's PATTERN, not just the first: read
            // one per arm, `"layout" | "windows" =>` hid the second verb and
            // shipped it undocumented with this test green. Stopping at `=>`
            // keeps a literal in an arm's BODY from counting as a verb; the
            // final arm's `format!` string is already out by the filter
            // above, which is a different exclusion doing different work.
            // Fail closed rather than skip. A pattern split across lines —
            // `"windows"` on one and `=> Request::Layout,` on the next — is
            // invisible to a scanner that reads one arm per line, and
            // skipping it hid a verb with this test green. The parser has one
            // arm per line; if that stops being true, this says so instead of
            // quietly reading less.
            let Some((pattern, _)) = line.split_once("=>") else {
                panic!("the parser's match no longer puts one arm on a line: {line}");
            };
            let mut rest = pattern;
            while let Some(open) = rest.find('"') {
                let Some(after) = rest.get(open + 1..) else {
                    break;
                };
                let Some((verb, tail)) = after.split_once('"') else {
                    break;
                };
                found += 1;
                assert!(
                    USAGE.iter().any(|(form, _)| form
                        .split_whitespace()
                        .next()
                        .is_some_and(|named| named == verb)),
                    "the parser takes '{verb}' and USAGE does not name it"
                );
                rest = tail;
            }
        }
        assert_eq!(
            found,
            USAGE.len(),
            "the parser and USAGE no longer carry the same number of verbs"
        );
    }

    #[test]
    fn the_client_tells_a_refusal_from_an_empty_room() {
        // Two statuses, because a typo and a session that is not running are
        // different things to have to fix.
        assert_eq!(split_answer("ok\n"), Ok(String::new()));
        assert_eq!(
            split_answer("ok\noutput width=1 height=2\n"),
            Ok("output width=1 height=2\n".to_string())
        );
        assert_eq!(
            split_answer("error no such request 'dance'\n"),
            Err(ControlFailure::Refused(
                "no such request 'dance'".to_string()
            ))
        );
        assert_eq!(
            ControlFailure::Refused(String::new()).exit_code(),
            2,
            "a refusal is not the same exit as an unreachable compositor"
        );
        assert_eq!(ControlFailure::Unreachable(String::new()).exit_code(), 1);
        // Anything that is not a status line is a socket that is not this
        // protocol, which is the unreachable answer rather than a refusal.
        assert!(matches!(
            split_answer("hello\n"),
            Err(ControlFailure::Unreachable(_))
        ));
        assert!(matches!(split_answer(""), Err(ControlFailure::Unreachable(_))));
    }
}
