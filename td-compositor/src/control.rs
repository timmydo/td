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
use crate::runtime::{Runtime, Sent};
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
    /// The same three orders, aimed at a window the caller NAMES rather than
    /// at whichever one happens to be focused.
    ///
    /// `layout` hands out an id per window and, until these existed, nothing
    /// consumed one: a caller could read the arrangement and then only act on
    /// the focused window, so "put the browser on 3" meant counting `focus
    /// right` presses and hoping nothing moved in between. Naming a window is
    /// not a capability the compositor lacked — the POINTER names one every
    /// time it clicks — it is one no keyboard could spell.
    FocusWindow(u64),
    SendWindow(u64, u8),
    MoveWindow(u64, Direction),
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
    (
        "send <@id> <1-9>",
        "move that window to that workspace, staying where you are",
    ),
    ("focus <left|right|up|down>", "focus the window that way"),
    (
        "focus <@id>",
        "focus that window, showing its workspace when it is not in view",
    ),
    ("move <left|right|up|down>", "move the focused window that way"),
    (
        "move <@id> <left|right|up|down>",
        "move that window that way, showing its workspace first",
    ),
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
            "send" => send_request(&mut words, verb)?,
            "focus" => focus_request(&mut words, verb)?,
            "move" => move_request(&mut words, verb)?,
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
            Request::FocusWindow(handle) => format!("focus {}", handle_word(handle)),
            Request::SendWindow(handle, number) => {
                format!("send {} {number}", handle_word(handle))
            }
            Request::MoveWindow(handle, direction) => {
                format!(
                    "move {} {}",
                    handle_word(handle),
                    direction_word(direction)
                )
            }
        }
    }
}

/// How a handle is written, in the report and in a request alike, so a caller
/// feeds one straight back without transforming it.
///
/// The `@` is what keeps the grammar unambiguous: `send <1-9>` sends the
/// FOCUSED window, so a bare number as a handle would make `send 7` mean
/// "send the focused window to workspace 7" to the parser and "send window 7
/// somewhere" to the person, with no way to tell which was meant.
fn handle_word(handle: u64) -> String {
    format!("@{handle}")
}

/// Whether a word NAMES a window rather than describing one.
///
/// An id BEGINS with `@` and nothing else in this grammar does — not a
/// direction, not a workspace number — so the overloaded verbs tell their two
/// forms apart by shape rather than by counting arguments. A word beginning
/// `@` that is not a well-formed id is an error and not a fall-through to the
/// other form, or `send @x 3` would complain about a workspace number.
///
/// `starts_with` and not `contains`: a malformed argument that happens to hold
/// an `@` somewhere is not an attempt to name a window, and swallowing it here
/// would answer `move 1@2 left` with a complaint about the id rather than
/// about the thing that is actually wrong with it.
///
/// The sigil is what makes the shape legible at all. `send <1-9>` already
/// means "send the FOCUSED window there", so a bare number would make
/// `send 7` ambiguous between that and "send window 7", and counting
/// arguments to resolve it would silently pick one reading of a request the
/// caller could not have known was ambiguous.
fn names_window(word: Option<&str>) -> bool {
    word.is_some_and(|word| word.starts_with('@'))
}

fn window(word: Option<&str>, verb: &str) -> Result<u64, String> {
    let word = word.ok_or_else(|| format!("'{verb}' needs a window id"))?;
    let bad = || format!("'{word}' is not a window id; ids look like @12");
    let digits = word.strip_prefix('@').ok_or_else(bad)?;
    // ONE spelling, which `u64::from_str` alone does not give: it accepts a
    // leading `+` and any number of leading zeros, so `@+5`, `@05` and `@5`
    // would be three ways to say one window, each arrived at silently. That is
    // the failure `@@5` is refused for, and worse — a caller pasting `@` onto
    // a padded or signed number never learns it was forgiven, and two callers
    // comparing notes disagree about what they addressed. So: digits, at least
    // one, and no leading zero. `@0` goes with them rather than resolving to
    // nothing, because 0 is not a name this compositor ever mints and telling
    // a caller its id is malformed beats telling it the window is gone.
    let leading_zero = digits.starts_with('0');
    if digits.is_empty()
        || leading_zero
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(bad());
    }
    digits.parse().map_err(|_| bad())
}

/// The overloaded verbs. Each is its own function so the parser's `match`
/// keeps ONE ARM PER LINE: the test that walks it reads arms off lines, and a
/// block arm would put this logic where that walk cannot see it. The verb
/// still appears on its arm, so nothing hides — only the argument's shape is
/// decided here.
fn focus_request<'a>(
    words: &mut impl Iterator<Item = &'a str>,
    verb: &str,
) -> Result<Request, String> {
    let word = words.next();
    if names_window(word) {
        return Ok(Request::FocusWindow(window(word, verb)?));
    }
    retired_here(word)?;
    Ok(Request::Focus(direction(word, verb)?))
}

fn send_request<'a>(
    words: &mut impl Iterator<Item = &'a str>,
    verb: &str,
) -> Result<Request, String> {
    let word = words.next();
    if names_window(word) {
        let handle = window(word, verb)?;
        return Ok(Request::SendWindow(handle, workspace(words.next(), verb)?));
    }
    retired_here(word)?;
    Ok(Request::Send(workspace(word, verb)?))
}

fn move_request<'a>(
    words: &mut impl Iterator<Item = &'a str>,
    verb: &str,
) -> Result<Request, String> {
    let word = words.next();
    if names_window(word) {
        let handle = window(word, verb)?;
        return Ok(Request::MoveWindow(handle, direction(words.next(), verb)?));
    }
    retired_here(word)?;
    Ok(Request::Move(direction(word, verb)?))
}

/// Refuse the address this channel used to take, where it could have been
/// written: the FIRST argument of a verb that took one.
///
/// `send 1:5 3` is a caller working from an old report or an old note, and
/// "1:5 is not a workspace number" sends it to correct the one word in the
/// line that was right. Asked only here and not inside the shared argument
/// parsers, because `send @1 1:5` and `workspace 1:5` never held an address
/// in that position — telling THEM about a retired form would be the same
/// misdirection the other way round.
fn retired_here(word: Option<&str>) -> Result<(), String> {
    match word.and_then(retired_address) {
        Some(retired) => Err(retired),
        None => Ok(()),
    }
}

/// Whether a word is shaped like the retired `client:object` address. Both
/// halves must be there and both must be digits: an empty half makes the
/// digit check vacuously true, and a word that never looked like an address
/// keeps its own complaint.
fn retired_address(word: &str) -> Option<String> {
    let (client, object) = word.split_once(':')?;
    if client.is_empty() || object.is_empty() {
        return None;
    }
    if !client.bytes().chain(object.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(format!(
        "'{word}' is the retired client:object address; windows are named \
         @12 now, and `layout` prints the one to use"
    ))
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
    /// What a caller ADDRESSES this window by, and what `id=` prints. Minted
    /// once per window and never reissued, so an id from a stale report names
    /// nothing rather than whatever reused its object number.
    pub handle: u64,
    /// The Wayland pair, reported as `object=` for diagnosis only. It is what
    /// the compositor keys its own scene by and what appears in its logs, so
    /// withholding it would make a report harder to line up against them —
    /// but it is not addressable, because it is exactly what a client may
    /// reuse.
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
    /// Drawn, reported, and NOT a leaf of any workspace tree: a portal dialog,
    /// which `Layout::float` takes out of the tiling while leaving it on
    /// screen. The addressed orders refuse one, so the report says which
    /// windows they are rather than leaving a caller to find out by being
    /// told a window it can see does not exist.
    pub floating: bool,
    /// The HANDLE of the window this one is constrained to, if any: an
    /// xdg-shell parent, an imported foreign one, or a portal's. A handle
    /// rather than a key because a caller acts on what it reads here, and the
    /// pair is not addressable. Reported because it changes what
    /// an addressed order DOES — the compositor keeps a family on one
    /// workspace and its topmost constrained child above its ancestor, so
    /// naming any member reaches the family rather than the member alone.
    /// Without the field a caller could see neither the relationship nor a
    /// reason for the answer it got.
    pub parent: Option<u64>,
    /// The client's own `xdg_toplevel.set_app_id`, or `None` from a client
    /// that set none. This is the field a caller can PREDICT: a handle is
    /// stable for as long as its window and longer, but it is still a number
    /// this compositor made up — it means nothing to a person and differs
    /// between two runs of the same session — so an agent told to put "the
    /// browser" somewhere has nothing else to match on.
    pub app_id: Option<String>,
    /// The client's own `xdg_toplevel.set_title`, or `None` from a client that
    /// has set none. NOT the app id, which is its own field beside this one:
    /// a client sets the two separately, and they answer different questions
    /// — a caller matching on what a window IS wants the app id, while one
    /// showing a person which window it is wants the title.
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
/// last means the reader can take the rest of the line and stop. It is one of
/// TWO fields a client chooses — `app_id` is the other — so it is one of the
/// two that could forge a record: a title carrying a newline would otherwise
/// print a `window` line of the client's own writing, and `clean_title` is
/// what stops that. `clean_app_id` is the same job at a different position —
/// a field that is not last has to be one word as well as one line.
///
/// A reader should take `title` as everything after `" title="`, with the
/// leading space, and split the rest on whitespace. An app id may contain
/// `=`, so a reader that hunts for the first `title=` anywhere in the line
/// can be steered to a client's own text — the one grammar trap a second
/// client-chosen field before the last one introduces.
pub fn report(snapshot: &ControlSnapshot) -> String {
    let mut out = String::new();
    // `windows` counts the records BELOW, so a reader can tell a report that
    // stopped early from a shorter one. The answer's body can be abandoned
    // part way — a deadline, or the socket's write timeout — and §15 named
    // that limit without closing it: the terminator check refuses a body that
    // stops mid-record, and this refuses one that stops between records.
    out.push_str(&format!(
        "output width={} height={} windows={}\n",
        snapshot.width,
        snapshot.height,
        snapshot.windows.len()
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
            "window id={} object={}:{} workspace={} x={} y={} width={} \
             height={} visible={} focused={} fullscreen={} floating={} \
             parent={} app_id={} title={}\n",
            handle_word(window.handle),
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
            window.floating,
            window.parent.map(handle_word).unwrap_or_default(),
            clean_app_id(window.app_id.as_deref()),
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

/// The app id, as ONE WORD.
///
/// `clean_title` may keep its spaces because a title is last on its line and
/// runs to the end of it. Every field before it has to be a single token or
/// the fields after stop being where a reader expects them, so whitespace is
/// replaced here as well as everything `reportable` refuses. An app id in the
/// wild is reverse-DNS and contains none; this is for the client that sends
/// one anyway, and for the same reason `clean_title` exists.
///
/// Empty is absent, which is `Scene::set_app_id`'s own rule — a client cannot
/// store an empty one — so `app_id=` says "this client set none" without a
/// second marker to explain.
fn clean_app_id(app_id: Option<&str>) -> String {
    let Some(app_id) = app_id else {
        return String::new();
    };
    app_id
        .chars()
        .map(|character| {
            if character.is_whitespace() || !reportable(character) {
                '_'
            } else {
                character
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
pub fn apply(runtime: &mut Runtime, request: Request) -> Result<Answer, String> {
    let command = match request {
        Request::Layout => return Ok(Answer::Report(report(&runtime.control_snapshot()))),
        Request::Workspace(number) => Command::SwitchWorkspace(number),
        Request::Send(number) => Command::MoveToWorkspace(number),
        Request::Focus(direction) => Command::Focus(direction),
        Request::Move(direction) => Command::Move(direction),
        Request::Fullscreen => Command::ToggleFullscreen,
        Request::Present(presentation) => Command::SetPresentation(presentation),
        Request::Group => Command::ToggleGrouped,
        // Aimed at a NAMED window, so each answers whether the window was
        // there. That is the caller's mistake and not the compositor's, which
        // is why it comes back as its own answer rather than as an `Err`: an
        // `Err` here means the session could not, and telling a caller to go
        // and look at the session because they typed a stale id is the wrong
        // instruction.
        Request::FocusWindow(handle) => {
            let Some(key) = runtime.window_for_handle(handle) else {
                return Ok(Answer::NoSuchWindow(handle));
            };
            let acted = runtime.control_focus(key)?;
            return found(runtime, handle, key, acted);
        }
        Request::SendWindow(handle, number) => {
            let Some(key) = runtime.window_for_handle(handle) else {
                return Ok(Answer::NoSuchWindow(handle));
            };
            return match runtime.control_send(key, number)? {
                Sent::Done => Ok(Answer::Ok),
                Sent::NoWindow => found(runtime, handle, key, false),
                // Floating is asked HERE rather than in the runtime because it
                // is a property of the answer and not of the move: the refusal
                // is the same either way, and only the sentence differs. The
                // NAME came with the root, since only the runtime can say
                // which ancestor is still nameable.
                Sent::FollowsParent { root, named } if runtime.is_floating(root) => {
                    Ok(Answer::FamilyStuck(handle, named))
                }
                Sent::FollowsParent { named, .. } => Ok(Answer::FollowsParent(handle, named)),
            };
        }
        Request::MoveWindow(handle, direction) => {
            let Some(key) = runtime.window_for_handle(handle) else {
                return Ok(Answer::NoSuchWindow(handle));
            };
            let acted = runtime.control_move(key, direction)?;
            return found(runtime, handle, key, acted);
        }
    };
    // `Runtime::command`, the keyboard's own path, whole-output damage and
    // all: a scripted tiling command is the same gesture as the typed one and
    // must not repair the screen a different amount.
    runtime.command(command)?;
    Ok(Answer::Ok)
}

/// What one request produced. Three cases because there are three answers on
/// the wire, and folding "no such window" into `Err` would have reported the
/// caller's mistake with the status that means the compositor's.
#[derive(Debug)]
pub enum Answer {
    Ok,
    Report(String),
    NoSuchWindow(u64),
    /// Named a window that IS there and cannot be arranged: a portal dialog.
    /// Its own answer because "no window" would be a lie about a window the
    /// report had just listed, and a caller cannot fix what it is not told.
    NotArrangeable(u64),
    /// Named a window that cannot leave its family's workspace alone. Carries
    /// the family's ROOT, because that is the window the caller should have
    /// named: an answer that withholds it makes the caller read the report
    /// again, and one that names the immediate parent of a child two deep
    /// hands back a window that would be refused for the same reason.
    FollowsParent(u64, u64),
    /// Named a window whose family cannot move at all, because the root it
    /// follows is itself unarrangeable — a portal dialog can be a parent, and
    /// `Layout::float` has taken it out of the tree. Its own answer so the
    /// caller learns that in ONE refusal: pointing at the root and letting
    /// them discover it is refused too is a remedy that is not one.
    FamilyStuck(u64, u64),
}

/// Which kind of "no" a refused order is.
///
/// Asked about the key the CALLER named, deliberately, even though `reveal`
/// may have acted on a descendant of it: the refusal prints this key, and
/// describing a different window than the one it names would answer a
/// question nobody asked. The two cannot disagree today — `topmost_parented`
/// follows only a mapped, overlapping, tiled child, which is never the
/// floating case below — so this is a rule to keep rather than a bug to fix.
fn found(
    runtime: &Runtime,
    handle: u64,
    key: SurfaceKey,
    acted: bool,
) -> Result<Answer, String> {
    if acted {
        return Ok(Answer::Ok);
    }
    // Which kind of "no" this is. A portal dialog is on screen and in the
    // report, so telling a caller it does not exist would be false about the
    // one thing the caller can see.
    if runtime.is_floating(key) {
        return Ok(Answer::NotArrangeable(handle));
    }
    Ok(Answer::NoSuchWindow(handle))
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
        Ok(Answer::Ok) => "ok\n".to_string(),
        Ok(Answer::Report(report)) => format!("ok\n{report}"),
        Ok(Answer::NoSuchWindow(handle)) => {
            format!("error no window {}\n", handle_word(handle))
        }
        Ok(Answer::NotArrangeable(handle)) => format!(
            "error window {} is floating and cannot be arranged\n",
            handle_word(handle)
        ),
        Ok(Answer::FollowsParent(child, root)) => format!(
            "error window {} follows {}; send that one\n",
            handle_word(child),
            handle_word(root)
        ),
        Ok(Answer::FamilyStuck(child, root)) => format!(
            "error window {} follows {}, which is floating and cannot be \
             arranged\n",
            handle_word(child),
            handle_word(root)
        ),
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
    split_answer(&answer, matches!(request, Request::Layout))
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

/// The `windows=` count the report's first line declares, if it declares one.
///
/// Read off the `output` line rather than counted from the records, because
/// the whole point is to have a number that came from the SENDER: a count
/// derived from what arrived would agree with what arrived no matter how much
/// of it was lost.
fn declared_windows(body: &str) -> Option<usize> {
    body.lines()
        .next()?
        .split_whitespace()
        .find_map(|field| field.strip_prefix("windows="))
        .and_then(|count| count.parse().ok())
}

/// Split a status line from its body.
///
/// The terminator is REQUIRED. `splitn` would hand back the whole answer as a
/// status when there is no newline in it, so a truncated `ok` — a compositor
/// killed mid-write, or something else entirely on the path — would exit 0 as
/// a success carrying no report. Nothing on this protocol writes a status
/// without a newline, so demanding one costs a caller nothing and turns a
/// half-answer into the failure it is.
///
/// `reported` is whether the REQUEST was one that owes a body, which only the
/// caller knows: an order's answer is `ok` and nothing else, and a question's
/// is a report. Judged without it, the emptiest truncation of all — a `layout`
/// answer cut to its status line — was indistinguishable from a `fullscreen`
/// that worked, so the one case the count exists to catch slipped through the
/// gap left for orders.
fn split_answer(answer: &str, reported: bool) -> Result<String, ControlFailure> {
    let Some((status, body)) = answer.split_once('\n') else {
        return Err(ControlFailure::Unreachable(format!(
            "control answer has no status line: '{answer}'"
        )));
    };
    if status == "ok" {
        // Every record the report writes ends in a newline, so a body that
        // does not is one the compositor gave up part way through — which
        // either the deadline or the socket's write timeout can cause. Same
        // argument as the status line's own terminator, one level down.
        if !body.is_empty() && !body.ends_with('\n') {
            return Err(ControlFailure::Unreachable(
                "control answer body is truncated".to_string(),
            ));
        }
        if !reported {
            // An order's answer is the status line and nothing else, so a body
            // here is an answer this client does not understand — and reading
            // one as success is how a caller comes to trust a compositor that
            // is saying something else entirely.
            if !body.is_empty() {
                return Err(ControlFailure::Unreachable(
                    "control answer carries a body an order does not".to_string(),
                ));
            }
            return Ok(String::new());
        }
        // A report owes all three of its record kinds. The bar's line is
        // derived from the same arrangement the windows are, so a report
        // without it is not a shorter report but a broken one — and it is the
        // one record the count is structurally unable to notice missing.
        //
        // Only this one is checked by shape. That the OUTPUT line comes first
        // is already required by the count below, which is read from line one
        // and found nowhere else, so a second check for it could not fail.
        if !body
            .lines()
            .nth(1)
            .is_some_and(|line| line.starts_with("workspace "))
        {
            return Err(ControlFailure::Unreachable(
                "control report carries no workspace line".to_string(),
            ));
        }
        // And the count, which is what catches the truncation the terminator
        // CANNOT see: a body abandoned cleanly between records is well formed
        // and merely short, so nothing about its shape says anything is
        // missing. Emitting `windows=` without reading it here would leave
        // that hole open while the design recorded it as closed.
        let Some(promised) = declared_windows(body) else {
            return Err(ControlFailure::Unreachable(
                "control answer body declares no window count".to_string(),
            ));
        };
        let carried = body
            .lines()
            .filter(|line| line.starts_with("window "))
            .count();
        if carried != promised {
            return Err(ControlFailure::Unreachable(format!(
                "control answer promised {promised} windows and carried {carried}"
            )));
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
    // Measured rather than guessed: a fixed column that the longest form
    // outgrows puts that one description somewhere no other line's is, which
    // is exactly the entry a reader most needs to line up.
    let column = USAGE.iter().map(|(form, _)| form.len()).max().unwrap_or(0);
    for (form, what) in USAGE {
        out.push_str(&format!("  {form:<column$} {what}\n"));
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
    use std::collections::BTreeSet;

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
            Request::FocusWindow(12),
            Request::SendWindow(12, 7),
            Request::MoveWindow(40, Direction::Right),
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

    /// One word of a USAGE form as a caller would actually type it. A literal
    /// stands for itself; `<a|b>` takes its first alternative, since the
    /// grammar has to accept all of them; and the two placeholders that are
    /// not alternations name a range and an id, whose spelling is the thing
    /// this is checking.
    fn example(word: &str) -> String {
        let Some(inner) = word.strip_prefix('<').and_then(|w| w.strip_suffix('>')) else {
            return word.to_string();
        };
        match inner {
            "1-9" => "1".to_string(),
            "@id" => "@12".to_string(),
            alternation => alternation
                .split('|')
                .next()
                .unwrap_or(alternation)
                .to_string(),
        }
    }

    #[test]
    fn the_usage_placeholders_are_spelled_the_way_the_parser_reads_them() {
        // `example` is what the check below leans on, so its own reading of a
        // form is pinned rather than assumed: an alternation that silently
        // produced the whole `<a|b>` word would make every form fail, and one
        // that produced an empty string would make several pass for nothing.
        assert_eq!(example("focus"), "focus");
        assert_eq!(example("<1-9>"), "1");
        assert_eq!(example("<@id>"), "@12");
        assert_eq!(example("<left|right|up|down>"), "left");
        assert!(Request::parse("focus @12").is_ok(), "the id example is not one");
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
        // And the other way: every USAGE entry parses AS WRITTEN, with each
        // placeholder filled in by its own spelling. Built from the verb
        // instead, this checked `send 1`, `focus left` and `move left` for all
        // eight entries, so the three addressed forms were never parsed at all
        // and USAGE could document an id syntax the parser refuses.
        for (form, _) in USAGE {
            let line: Vec<String> = form.split_whitespace().map(example).collect();
            let line = line.join(" ");
            assert!(
                Request::parse(&line).is_ok(),
                "help names '{form}' but '{line}' does not parse"
            );
        }
        // Every FORM, not every verb. Checked by verb, deleting all three
        // addressed entries left this green: `focus`, `send` and `move` were
        // still named by their unaddressed entries, so the half of the
        // vocabulary a caller cannot guess could go undocumented without a
        // test noticing. An addressed form is the half that needs the help
        // most — nothing about a window suggests it is spelled `@12`.
        for verb in ["send", "focus", "move"] {
            assert!(
                USAGE.iter().any(|(form, _)| {
                    form.starts_with(verb) && form.contains("<@id>")
                }),
                "the help text does not document the addressed '{verb}'"
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
                handle: 9,
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
                floating: false,
                parent: None,
                app_id: None,
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
                    handle: 11,
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
                    floating: false,
                    parent: None,
                    app_id: Some("org.td.term".to_string()),
                    title: Some("shell".to_string()),
                },
                ControlWindow {
                    handle: 12,
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
                    floating: false,
                    parent: None,
                    app_id: None,
                    title: None,
                },
            ],
        };
        assert_eq!(
            report(&snapshot),
            "output width=800 height=600 windows=2\n\
             workspace active=2 occupied=1,2\n\
             window id=@11 object=1:5 workspace=1 x=0 y=24 width=800 \
             height=576 visible=false focused=false fullscreen=false \
             floating=false parent= app_id=org.td.term title=shell\n\
             window id=@12 object=2:9 workspace=2 x=0 y=24 width=400 \
             height=576 visible=true focused=true fullscreen=false \
             floating=false parent= app_id= title=\n"
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

    /// A session with more than one window, which is what an ADDRESSED order
    /// needs: with one window on screen, acting on the focused one and acting
    /// on the one you named are the same act and prove nothing apart.
    fn session_of(windows: &[SurfaceKey]) -> (Arc<Mutex<Runtime>>, Cleanup) {
        let frame = temporary("control-fb");
        let framebuffer = crate::framebuffer::Framebuffer::test_file(&frame.0, 240, 600, 240 * 4)
            .expect("test framebuffer");
        let mut runtime = Runtime::new(framebuffer);
        for window in windows {
            runtime
                .commit(
                    *window,
                    crate::buffer::Surface::from_shm_pixels(
                        100,
                        100,
                        [1u8, 2, 3, 0].repeat(10_000),
                        crate::scene::SHM_XRGB8888,
                    )
                    .expect("test surface"),
                )
                .expect("commit");
        }
        (Arc::new(Mutex::new(runtime)), frame)
    }

    #[test]
    fn the_fixture_mints_handles_in_the_order_it_commits() {
        // Every wire literal in this module writes a handle by hand — `focus
        // @1` for the first window a fixture commits. That is only readable
        // because minting is in commit order, and only SAFE because this says
        // so: a change to where handles are minted would otherwise leave the
        // suite addressing whichever windows the new order happened to name,
        // with each test still green about the wrong one.
        let first = SurfaceKey {
            client: 1,
            object: 1,
        };
        let second = SurfaceKey {
            client: 1,
            object: 2,
        };
        let third = SurfaceKey {
            client: 2,
            object: 2,
        };
        let (runtime, _frame) = session_of(&[first, second, third]);
        let report = layout_of(&runtime);
        assert_eq!(handle_of(&report, first), 1, "{report}");
        assert_eq!(handle_of(&report, second), 2, "{report}");
        assert_eq!(handle_of(&report, third), 3, "{report}");
    }

    fn layout_of(runtime: &Mutex<Runtime>) -> String {
        let answered = answer(runtime, "layout");
        answered
            .strip_prefix("ok\n")
            .unwrap_or_else(|| panic!("layout did not answer ok: {answered}"))
            .to_string()
    }

    fn windows_in(report: &str) -> Vec<&str> {
        report
            .lines()
            .filter(|line| line.starts_with("window "))
            .collect()
    }

    fn record_for(report: &str, handle: u64) -> String {
        let wanted = format!("window id=@{handle} ");
        windows_in(report)
            .into_iter()
            .find(|line| line.starts_with(&wanted))
            .unwrap_or_else(|| panic!("no record for @{handle} in\n{report}"))
            .to_string()
    }

    /// The handle the report gives a window a test committed by key.
    ///
    /// Read out of the report rather than predicted from mint order, because
    /// that is what a caller does and because a test that hard-coded the order
    /// would pass while the report named something else entirely.
    fn handle_of(report: &str, key: SurfaceKey) -> u64 {
        // The FIELD, not a substring: `title` runs to the end of the line and
        // a client picks it, so a window titled `object=1:1` would otherwise
        // answer for one it is not. This module has a test about exactly that
        // kind of forgery, so its own helper should not be forgeable.
        let wanted = format!("object={}:{}", key.client, key.object);
        windows_in(report)
            .into_iter()
            .find(|line| line.split_whitespace().nth(2) == Some(wanted.as_str()))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|field| field.strip_prefix("id=@"))
            .and_then(|handle| handle.parse().ok())
            .unwrap_or_else(|| {
                panic!("no handle for {}:{} in\n{report}", key.client, key.object)
            })
    }

    #[test]
    fn an_order_reaches_the_window_it_names_and_not_the_focused_one() {
        // The whole point of the addressed forms. `layout` handed out an id
        // per window and nothing consumed one, so a caller could read the
        // arrangement and then only act on whatever was focused — "put the
        // browser on 3" meant counting `focus right` presses and hoping
        // nothing moved in between.
        let first = SurfaceKey {
            client: 1,
            object: 1,
        };
        let second = SurfaceKey {
            client: 2,
            object: 2,
        };
        let (runtime, _frame) = session_of(&[first, second]);
        let socket = temporary("control-sock");
        serve(&socket.0, Arc::clone(&runtime)).expect("serve");

        // The LAST committed window is the focused one, so `first` is
        // deliberately not it: an order that quietly acted on the focused
        // window would move `second` and this would catch it.
        let before = ask(&socket.0, Request::Layout).expect("layout");
        assert!(
            record_for(&before, handle_of(&before, second)).contains("focused=true"),
            "the fixture does not focus the window this test assumes:\n{before}"
        );

        let named = handle_of(&before, first);
        ask(&socket.0, Request::SendWindow(named, 3)).expect("send");
        let after = ask(&socket.0, Request::Layout).expect("layout");
        assert!(
            record_for(&after, handle_of(&after, first)).contains("workspace=3"),
            "the named window did not move:\n{after}"
        );
        assert!(
            record_for(&after, handle_of(&after, second)).contains("workspace=1"),
            "the order moved the focused window instead:\n{after}"
        );
        // And the view did not follow it, which is `send`'s own rule.
        assert!(
            after.contains("workspace active=1 "),
            "sending a window moved the view:\n{after}"
        );
    }

    #[test]
    fn focusing_a_named_window_shows_the_workspace_it_is_on() {
        // `Layout::focus_key` only takes a leaf of the SHOWN workspace, so the
        // alternative to switching is not "focus it where it is" — it is
        // refusing. Focus that leaves the window invisible is not focus.
        let first = SurfaceKey {
            client: 1,
            object: 1,
        };
        let second = SurfaceKey {
            client: 2,
            object: 2,
        };
        let (runtime, _frame) = session_of(&[first, second]);
        let socket = temporary("control-sock");
        serve(&socket.0, Arc::clone(&runtime)).expect("serve");

        let listed = ask(&socket.0, Request::Layout).expect("layout");
        let named = handle_of(&listed, first);
        ask(&socket.0, Request::SendWindow(named, 4)).expect("send");
        ask(&socket.0, Request::FocusWindow(named)).expect("focus");
        let after = ask(&socket.0, Request::Layout).expect("layout");
        assert!(
            after.contains("workspace active=4 "),
            "focusing a hidden window did not show its workspace:\n{after}"
        );
        assert!(
            record_for(&after, handle_of(&after, first)).contains("focused=true"),
            "the named window is not the focused one:\n{after}"
        );
    }

    #[test]
    fn a_window_that_is_not_there_is_the_callers_mistake() {
        // `error`, not `unavailable`: a stale id is the caller's to fix, and
        // sending a script to go and look at the session for it is the wrong
        // instruction — the distinction the third status exists to draw.
        let window = SurfaceKey {
            client: 1,
            object: 1,
        };
        let (runtime, _frame) = session_of(&[window]);
        // 99 is a handle this session has not minted YET. It is not reserved
        // against the future — mint 99 windows and it names one — and that is
        // exactly right: what a handle promises is that it never comes BACK,
        // not that a number nobody has been given stays unusable forever.
        assert_eq!(
            answer(&runtime, "focus @99"),
            "error no window @99\n",
            "a stale id was reported as a broken compositor"
        );
        assert_eq!(answer(&runtime, "send @99 2"), "error no window @99\n");
        assert_eq!(answer(&runtime, "move @99 left"), "error no window @99\n");
        // And the client turns that into the refusal exit, not the absent one.
        assert_eq!(
            split_answer(&answer(&runtime, "focus @99"), false),
            Err(ControlFailure::Refused("no window @99".to_string()))
        );
    }

    #[test]
    fn focusing_a_named_window_actually_moves_the_focus() {
        // On the workspace ALREADY in view, which is the commonest case and
        // was the unguarded one: the first version of this test sent the
        // window to an empty workspace first, and `Layout::map` focuses what
        // it maps, so a bare workspace switch satisfied `focused=true` and
        // deleting the focus call left the suite green.
        let first = SurfaceKey {
            client: 1,
            object: 1,
        };
        let second = SurfaceKey {
            client: 2,
            object: 2,
        };
        let (runtime, _frame) = session_of(&[first, second]);
        let before = layout_of(&runtime);
        assert!(
            record_for(&before, handle_of(&before, second)).contains("focused=true"),
            "the fixture does not focus the window this test assumes:\n{before}"
        );

        assert_eq!(answer(&runtime, "focus @1"), "ok\n");
        let after = layout_of(&runtime);
        assert!(
            record_for(&after, handle_of(&after, first)).contains("focused=true"),
            "focus did not move to the named window:\n{after}"
        );
        assert!(
            record_for(&after, handle_of(&after, second)).contains("focused=false"),
            "two windows are focused at once:\n{after}"
        );
        // And the view did not wander, since it was already the right one.
        assert!(after.contains("workspace active=1 "), "{after}");
    }

    #[test]
    fn a_named_window_behind_a_fullscreen_one_is_revealed_not_lied_about() {
        // `Layout::focus_key` refuses a leaf on a workspace where a DIFFERENT
        // window is fullscreen. Composed by hand, that refusal was discarded:
        // the view moved, the named window stayed hidden and unfocused, and
        // the caller was told `ok` — the one answer wrong under every reading.
        // `activate_key` is the launcher's path and answers it, because
        // revealing a named window is an explicit request to see it.
        let hidden = SurfaceKey {
            client: 1,
            object: 1,
        };
        let covering = SurfaceKey {
            client: 2,
            object: 2,
        };
        let (runtime, _frame) = session_of(&[hidden, covering]);
        // `covering` is focused, so this makes it the workspace's fullscreen.
        assert_eq!(answer(&runtime, "fullscreen"), "ok\n");
        let before = layout_of(&runtime);
        assert!(
            record_for(&before, handle_of(&before, covering)).contains("fullscreen=true"),
            "the fixture did not go fullscreen:\n{before}"
        );

        assert_eq!(answer(&runtime, "focus @1"), "ok\n");
        let after = layout_of(&runtime);
        assert!(
            record_for(&after, handle_of(&after, hidden)).contains("focused=true"),
            "the named window was not focused:\n{after}"
        );
        assert!(
            record_for(&after, handle_of(&after, hidden)).contains("visible=true"),
            "the named window was focused but left invisible:\n{after}"
        );
        assert!(
            record_for(&after, handle_of(&after, covering)).contains("fullscreen=false"),
            "the covering window kept a fullscreen that hides the named one:\n{after}"
        );
    }

    #[test]
    fn a_window_the_layout_remembers_but_does_not_place_is_not_addressable() {
        // `homes` deliberately REMEMBERS an unmapped window so a client that
        // maps again lands where it was, so asking it alone answers for
        // windows that are in no tree. Asked that way, an order aimed at an
        // unmapped window reported `ok` for doing nothing — and, for `focus`,
        // moved the view to a workspace to show a window that is not there.
        //
        // Object ids apart from the handles, so `@1` in the three refusals
        // below is the handle and cannot be the object number.
        let window = SurfaceKey {
            client: 1,
            object: 51,
        };
        let other = SurfaceKey {
            client: 2,
            object: 52,
        };
        let (runtime, _frame) = session_of(&[window, other]);
        {
            let mut locked = runtime.lock().expect("runtime");
            locked.unmap(window).expect("unmap");
            assert_eq!(
                locked.control_snapshot().windows.len(),
                1,
                "the fixture did not actually unmap the window"
            );
        }
        assert_eq!(
            answer(&runtime, "focus @1"),
            "error no window @1\n",
            "an unmapped window was reported as addressable"
        );
        assert_eq!(answer(&runtime, "send @1 3"), "error no window @1\n");
        // `move` too. It is the one this path can most easily lose: the handle
        // RESOLVES here — the window is unmapped, not gone — so the refusal
        // has to come from `reveal` further down rather than from the lookup.
        assert_eq!(answer(&runtime, "move @1 left"), "error no window @1\n");
    }

    #[test]
    fn a_handle_whose_window_was_replaced_addresses_neither() {
        // The failure the whole migration is for, seen from the wire. A client
        // may destroy a surface and be handed the same object id for the next
        // one, so under the old address a caller holding `1:1` from an earlier
        // report would silently have started ordering a DIFFERENT window
        // about. Here the old name must find nothing, the new window must be
        // untouched, and every addressed verb must agree — a resolver that
        // fell back to "the nearest live window" would be invisible to a test
        // that only asks after an empty session.
        let key = SurfaceKey {
            client: 1,
            object: 1,
        };
        let bystander = SurfaceKey {
            client: 2,
            object: 2,
        };
        let (runtime, _frame) = session_of(&[key, bystander]);
        let retired = handle_of(&layout_of(&runtime), key);
        {
            let mut locked = runtime.lock().expect("runtime");
            locked.remove(key).expect("remove");
            locked
                .commit(
                    key,
                    crate::buffer::Surface::from_shm_pixels(
                        100,
                        100,
                        [1u8, 2, 3, 0].repeat(10_000),
                        crate::scene::SHM_XRGB8888,
                    )
                    .expect("test surface"),
                )
                .expect("commit");
        }
        let report = layout_of(&runtime);
        let live = handle_of(&report, key);
        assert_ne!(live, retired, "the replacement inherited a dead name:\n{report}");

        // Every verb, because each resolves the handle in its own arm.
        for order in [
            format!("focus @{retired}"),
            format!("send @{retired} 3"),
            format!("move @{retired} left"),
        ] {
            assert_eq!(
                answer(&runtime, &order),
                format!("error no window @{retired}\n"),
                "'{order}' reached a window"
            );
        }

        // And the replacement is where it was, on the workspace it was
        // committed to rather than the 3 the stale `send` asked for.
        let after = layout_of(&runtime);
        assert!(
            record_for(&after, live).contains("workspace=1 "),
            "a stale order moved the replacement:\n{after}"
        );
        assert!(
            record_for(&after, handle_of(&after, bystander)).contains("workspace=1 "),
            "a stale order moved a bystander:\n{after}"
        );
    }

    #[test]
    fn a_dialog_on_screen_is_refused_by_name_not_called_nonexistent() {
        // A portal dialog is drawn, routed input, and listed by `layout` —
        // and `Layout::float` takes it out of the tiling tree, so every
        // addressed order refuses it. Answering that refusal with "no window"
        // told a caller the id the report had just handed it was imaginary,
        // which is the one thing it could not act on: there is nothing to
        // spell differently. So the report says which windows those are, and
        // the refusal says why.
        //
        // Object ids apart from the handles: `@2` in the refusal below can
        // only be the dialog's handle, never its Wayland object number.
        let parent = SurfaceKey {
            client: 1,
            object: 41,
        };
        let dialog = SurfaceKey {
            client: 2,
            object: 42,
        };
        let (runtime, _frame) = session_of(&[parent, dialog]);
        {
            let mut locked = runtime.lock().expect("runtime");
            locked
                .export_foreign_toplevel(parent, "dialog-parent".to_string())
                .expect("export");
            let manager = crate::runtime::PortalManagerIdentity {
                client: 2,
                object: 20,
                generation: 1,
            };
            assert_eq!(
                locked.set_portal_parent(dialog, "dialog-parent", manager, 1),
                Ok(Some(parent)),
                "the fixture did not make a portal dialog"
            );
        }

        let report = layout_of(&runtime);
        assert!(
            record_for(&report, handle_of(&report, dialog)).contains("floating=true"),
            "the dialog is not reported as floating:\n{report}"
        );
        assert!(
            record_for(&report, handle_of(&report, parent)).contains("floating=false"),
            "a tiled window is reported as floating:\n{report}"
        );

        let refused = "error window @2 is floating and cannot be arranged\n";
        assert_eq!(answer(&runtime, "focus @2"), refused);
        assert_eq!(answer(&runtime, "send @2 3"), refused);
        assert_eq!(answer(&runtime, "move @2 left"), refused);
        // And a window that really is absent still gets the other answer, so
        // the two refusals stay apart rather than one swallowing the other.
        assert_eq!(answer(&runtime, "focus @99"), "error no window @99\n");
    }

    /// A parent and the child constrained to it, both mapped and grouped.
    fn family(runtime: &Mutex<Runtime>, parent: SurfaceKey, child: SurfaceKey) {
        let mut locked = runtime.lock().expect("runtime");
        assert_eq!(
            locked.set_local_parent(child, Some(parent)),
            Ok(Some(parent)),
            "the fixture did not make a family"
        );
    }

    /// What is on the glass. The report cannot answer this: it is computed
    /// from the tree rather than read off the published map, deliberately, so
    /// it says what the arrangement IS and not what was painted.
    fn screen(frame: &Cleanup) -> Vec<u8> {
        std::fs::read(&frame.0).expect("framebuffer")
    }

    #[test]
    fn an_addressed_move_actually_moves_the_window() {
        // The success path of `move <id> <direction>`, which had no test at
        // all: deleting `self.command(Command::Move(direction))` from
        // `control_move` left every one of the crate's tests green, since the
        // three that mention the addressed form all pin refusals.
        let first = SurfaceKey {
            client: 1,
            object: 1,
        };
        let second = SurfaceKey {
            client: 2,
            object: 2,
        };
        let (runtime, _frame) = session_of(&[first, second]);
        let before = layout_of(&runtime);
        let left_of = |report: &str, key: SurfaceKey| -> i32 {
            let record = record_for(report, handle_of(report, key));
            record
                .split_whitespace()
                .find_map(|field| field.strip_prefix("x="))
                .and_then(|x| x.parse().ok())
                .unwrap_or_else(|| panic!("no x in {record}"))
        };
        assert!(
            left_of(&before, first) < left_of(&before, second),
            "the fixture does not order the windows as this test assumes:\n{before}"
        );

        assert_eq!(answer(&runtime, "move @1 right"), "ok\n");
        let after = layout_of(&runtime);
        assert!(
            left_of(&after, first) > left_of(&after, second),
            "the named window did not move:\n{after}"
        );
    }

    #[test]
    fn an_addressed_order_reaches_the_glass_and_not_only_the_tree() {
        // The report is computed from the arrangement, so it says what the
        // layout IS whether or not anything was painted — which means every
        // assertion above this one would hold with the settle deleted. It
        // was: removing `self.settle(true)?` from `control_focus` and from
        // `control_send` left all of them green, and the screen kept showing
        // the old focus until something else repainted.
        let first = SurfaceKey {
            client: 1,
            object: 1,
        };
        let second = SurfaceKey {
            client: 2,
            object: 2,
        };
        let (runtime, frame) = session_of(&[first, second]);
        // A paint to compare against: the fixture's own commits have already
        // put something on the glass.
        let before = screen(&frame);
        assert!(before.iter().any(|byte| *byte != 0), "nothing was painted");

        // Focus moves the decoration, so the pixels have to change.
        assert_eq!(answer(&runtime, "focus @1"), "ok\n");
        let focused = screen(&frame);
        assert_ne!(
            focused, before,
            "an addressed focus changed the tree and not the screen"
        );

        // And a send, whose window leaves the shown workspace entirely.
        assert_eq!(answer(&runtime, "send @1 4"), "ok\n");
        let sent = screen(&frame);
        assert_ne!(
            sent, focused,
            "an addressed send changed the tree and not the screen"
        );
    }

    #[test]
    fn a_named_window_reaches_its_family_the_way_a_click_does() {
        // `reveal` follows `topmost_parented`, so naming the PARENT focuses
        // the constrained child on top of it. That is not this channel
        // inventing something: `focus_surface`, which is where a pointer click
        // lands, follows exactly the same chain, and `enforce_parent_focus`
        // would put focus back on the child anyway. Pinned because it is the
        // one place an addressed order acts on a window other than the one
        // named, and a caller reading `ok` deserves the rule written down.
        //
        // The parent's OBJECT ID is deliberately not its handle. Every fixture
        // in this module used to commit `1:1` first, so `parent=@1` held just
        // as well for a report that printed the Wayland object number — the
        // one thing `parent=` must not be. Object 7, committed first, is still
        // handle 1, and the two numbers can no longer be confused.
        let parent = SurfaceKey {
            client: 1,
            object: 7,
        };
        let child = SurfaceKey {
            client: 1,
            object: 2,
        };
        let elsewhere = SurfaceKey {
            client: 2,
            object: 2,
        };
        let (runtime, _frame) = session_of(&[parent, child, elsewhere]);
        family(&runtime, parent, child);
        // GROUPED, because that is the whole condition: `topmost_parented`
        // follows a child only where the two overlap, so side-by-side tiles
        // are unaffected and `focus <parent>` focuses the parent named.
        assert_eq!(answer(&runtime, "group"), "ok\n");
        assert_eq!(answer(&runtime, "focus @3"), "ok\n");

        assert_eq!(answer(&runtime, "focus @1"), "ok\n");
        let after = layout_of(&runtime);
        assert!(
            record_for(&after, handle_of(&after, child)).contains("focused=true"),
            "the family constraint was not applied:\n{after}"
        );
        // And the report says why, so the answer is predictable from it: the
        // child names its parent.
        assert!(
            record_for(&after, handle_of(&after, child)).contains("parent=@1 "),
            "the child does not name its parent:\n{after}"
        );
        assert!(
            record_for(&after, handle_of(&after, parent)).contains("parent= "),
            "a window with no parent claims one:\n{after}"
        );
    }

    #[test]
    fn an_addressed_move_on_a_family_moves_the_window_the_click_would() {
        // `reveal`'s `topmost_parented` was unpinned: deleting it left every
        // test green, because the `focus` case is over-determined —
        // `enforce_parent_focus` re-imposes inside the settle what `reveal`
        // had already chosen, so the assertion held either way. `move` is
        // where the two diverge, since `Command::Move` acts on the focused
        // window BEFORE any settle can correct the choice. Whichever window
        // `reveal` picks is the window that moves, so this is the test that
        // says which.
        let parent = SurfaceKey {
            client: 1,
            object: 1,
        };
        let child = SurfaceKey {
            client: 1,
            object: 2,
        };
        let other = SurfaceKey {
            client: 2,
            object: 2,
        };
        let (runtime, _frame) = session_of(&[parent, child, other]);
        family(&runtime, parent, child);
        assert_eq!(answer(&runtime, "group"), "ok\n");

        assert_eq!(answer(&runtime, "move @1 right"), "ok\n");
        let after = layout_of(&runtime);
        // The CHILD left the group, which is what clicking the parent and
        // pressing the chord does: the click lands on the child above it.
        assert!(
            record_for(&after, handle_of(&after, child)).contains("parent=@1 "),
            "the fixture lost the relationship:\n{after}"
        );
        let order: Vec<&str> = windows_in(&after)
            .into_iter()
            .filter_map(|line| line.split_whitespace().nth(1))
            .collect();
        assert_eq!(
            order,
            vec!["id=@1", "id=@3", "id=@2"],
            "a different window moved than the pointer would have moved"
        );
    }

    #[test]
    fn sending_a_window_where_it_already_is_is_not_a_failure() {
        // A caller that asks for a state and gets it has succeeded, and a
        // script that reaches a state then confirms it must not be told its
        // own success was an error. Unpinned before: deleting the check
        // turned this answer into `unavailable layout refused to send …`,
        // and no test noticed.
        let window = SurfaceKey {
            client: 1,
            object: 1,
        };
        let other = SurfaceKey {
            client: 2,
            object: 2,
        };
        let (runtime, _frame) = session_of(&[window, other]);
        assert_eq!(answer(&runtime, "send @1 1"), "ok\n");
        let after = layout_of(&runtime);
        assert!(
            record_for(&after, handle_of(&after, window)).contains("workspace=1 "),
            "the window that was already there moved:\n{after}"
        );

        // And the same for a window that is family, which is the case the
        // ordering turns on: a family shares a workspace, so asking for the
        // one it is on is always the already-there case and must not be
        // refused for a move that was never needed.
        let child = SurfaceKey {
            client: 1,
            object: 3,
        };
        {
            let mut locked = runtime.lock().expect("runtime");
            locked
                .commit(
                    child,
                    crate::buffer::Surface::from_shm_pixels(
                        100,
                        100,
                        [1u8, 2, 3, 0].repeat(10_000),
                        crate::scene::SHM_XRGB8888,
                    )
                    .expect("test surface"),
                )
                .expect("commit");
        }
        family(&runtime, window, child);
        assert_eq!(answer(&runtime, "send @3 1"), "ok\n");
    }

    #[test]
    fn the_family_refusal_names_a_window_that_can_actually_move() {
        // The refusal is the caller's only stated remedy, so it has to be one
        // that works. Naming the immediate parent of a child two deep hands
        // back a window refused for the same reason — a second dead end — so
        // it names the ROOT of the chain, which has no parent to follow.
        //
        // The object ids are deliberately not 1, 2, 3. Committed in this
        // order the handles ARE 1, 2, 3, so a fixture numbered to match would
        // let a refusal print the Wayland object id and still read `@1` —
        // the one number that must not appear where a handle belongs.
        let root = SurfaceKey {
            client: 1,
            object: 11,
        };
        let middle = SurfaceKey {
            client: 1,
            object: 12,
        };
        let leaf = SurfaceKey {
            client: 1,
            object: 13,
        };
        let (runtime, _frame) = session_of(&[root, middle, leaf]);
        family(&runtime, root, middle);
        family(&runtime, middle, leaf);

        assert_eq!(
            answer(&runtime, "send @3 3"),
            "error window @3 follows @1; send that one\n"
        );
        // And the remedy works, taking the whole chain with it.
        assert_eq!(answer(&runtime, "send @1 3"), "ok\n");
        let after = layout_of(&runtime);
        for window in [root, middle, leaf] {
            assert!(
                record_for(&after, handle_of(&after, window)).contains("workspace=3 "),
                "the family did not travel together:\n{after}"
            );
        }
    }

    #[test]
    fn a_family_that_cannot_move_at_all_says_so_in_one_refusal() {
        // A portal dialog can be a PARENT — DESIGN §"A floating portal parent
        // supplies its retained workspace" contemplates exactly that — and
        // `Layout::float` has taken it out of the tree. So the family root a
        // refusal points at can itself be unarrangeable, and naming it would
        // hand the caller a second, different refusal and no window that
        // works. One answer says the whole thing instead.
        // Object ids that are not the handles: committed in this order the
        // child is `@1` and the dialog `@2`, so a refusal that printed a
        // Wayland object number instead would now say `@21` and be caught.
        let dialog = SurfaceKey {
            client: 2,
            object: 22,
        };
        let child = SurfaceKey {
            client: 1,
            object: 21,
        };
        let (runtime, _frame) = session_of(&[child, dialog]);
        {
            let mut locked = runtime.lock().expect("runtime");
            let manager = crate::runtime::PortalManagerIdentity {
                client: 2,
                object: 20,
                generation: 1,
            };
            // No export by that handle, so the dialog stands alone: floated,
            // and with no parent of its own to be the root instead.
            assert_eq!(
                locked.set_portal_parent(dialog, "no-such-handle", manager, 1),
                Ok(None),
                "the fixture gave the dialog a parent"
            );
            assert_eq!(
                locked.set_local_parent(child, Some(dialog)),
                Ok(Some(dialog)),
                "the fixture did not parent the child to the dialog"
            );
        }
        let report = layout_of(&runtime);
        assert!(
            record_for(&report, handle_of(&report, dialog)).contains("floating=true"),
            "the fixture's root is not floating:\n{report}"
        );
        assert!(
            record_for(&report, handle_of(&report, child)).contains("parent=@2 "),
            "the fixture's child does not follow the dialog:\n{report}"
        );

        assert_eq!(
            answer(&runtime, "send @1 3"),
            "error window @1 follows @2, which is floating and cannot be \
             arranged\n"
        );
    }

    #[test]
    fn side_by_side_tiles_are_not_a_family_to_follow() {
        // The other half of the rule above, and the commoner arrangement:
        // `topmost_parented` follows a child only where the two OVERLAP, so a
        // parent tiled beside its child is focused as named. Pinned so a
        // widening of the follow rule cannot pass as the existing behaviour.
        let parent = SurfaceKey {
            client: 1,
            object: 1,
        };
        let child = SurfaceKey {
            client: 1,
            object: 2,
        };
        let (runtime, _frame) = session_of(&[parent, child]);
        family(&runtime, parent, child);

        assert_eq!(answer(&runtime, "focus @1"), "ok\n");
        let after = layout_of(&runtime);
        assert!(
            record_for(&after, handle_of(&after, parent)).contains("focused=true"),
            "a split tile followed its child anyway:\n{after}"
        );
    }

    #[test]
    fn a_window_that_cannot_leave_its_parent_is_told_so_not_told_ok() {
        // `enforce_parent_layout` runs inside the `settle` an addressed `send`
        // performs, and it puts a mapped child back on its parent's workspace.
        // So the move was made and unmade inside one request while the caller
        // was told `ok` — the window it asked about had not moved, and reading
        // the report afterwards was the only way to find out.
        //
        // Object ids apart from the handles, so `@1`/`@2` in the refusal below
        // can only have come from the handle map.
        let parent = SurfaceKey {
            client: 1,
            object: 31,
        };
        let child = SurfaceKey {
            client: 1,
            object: 32,
        };
        let (runtime, _frame) = session_of(&[parent, child]);
        family(&runtime, parent, child);

        assert_eq!(
            answer(&runtime, "send @2 3"),
            "error window @2 follows @1; send that one\n"
        );
        let after = layout_of(&runtime);
        assert!(
            record_for(&after, handle_of(&after, child)).contains("workspace=1 "),
            "the refused send moved the window anyway:\n{after}"
        );

        // And the instruction the refusal gives actually works: sending the
        // PARENT takes the family, because the same repair follows it there.
        assert_eq!(answer(&runtime, "send @1 3"), "ok\n");
        let moved = layout_of(&runtime);
        assert!(
            record_for(&moved, handle_of(&moved, parent)).contains("workspace=3 "),
            "the parent did not move:\n{moved}"
        );
        assert!(
            record_for(&moved, handle_of(&moved, child)).contains("workspace=3 "),
            "the child did not follow its parent:\n{moved}"
        );
    }

    #[test]
    fn a_window_whose_parent_is_gone_can_be_sent_after_all() {
        // The refusal above holds only while the relationship does, and it is
        // the compositor that ends one: `reparent_around_unmap` runs on every
        // unmap, so closing the parent leaves the child with no parent to
        // follow. Pinned because the refusal reads `toplevel_parent` directly
        // and would otherwise be trusting that rule silently — a window held
        // by a family that no longer exists could not be moved at all, and
        // the report would show no reason, an unmapped parent being absent
        // from it.
        let parent = SurfaceKey {
            client: 1,
            object: 1,
        };
        let child = SurfaceKey {
            client: 1,
            object: 2,
        };
        let (runtime, _frame) = session_of(&[parent, child]);
        family(&runtime, parent, child);
        runtime
            .lock()
            .expect("runtime")
            .unmap(parent)
            .expect("unmap");

        assert_eq!(answer(&runtime, "send @2 3"), "ok\n");
        let after = layout_of(&runtime);
        assert!(
            record_for(&after, handle_of(&after, child)).contains("workspace=3 "),
            "the child was held by a parent that is gone:\n{after}"
        );
    }

    #[test]
    fn a_dismissed_dialog_is_gone_rather_than_floating() {
        // `unmap` leaves portal membership alone, so asking the set alone said
        // "floating" about a window that is not on screen and not in the
        // report. A caller told that would go looking for a window to close.
        let parent = SurfaceKey {
            client: 1,
            object: 1,
        };
        let dialog = SurfaceKey {
            client: 2,
            object: 2,
        };
        let (runtime, _frame) = session_of(&[parent, dialog]);
        {
            let mut locked = runtime.lock().expect("runtime");
            locked
                .export_foreign_toplevel(parent, "gone-parent".to_string())
                .expect("export");
            let manager = crate::runtime::PortalManagerIdentity {
                client: 2,
                object: 20,
                generation: 1,
            };
            assert_eq!(
                locked.set_portal_parent(dialog, "gone-parent", manager, 1),
                Ok(Some(parent))
            );
            locked.unmap(dialog).expect("unmap");
        }
        let report = layout_of(&runtime);
        assert!(
            !report.contains("id=@2 "),
            "an unmapped dialog is still reported:\n{report}"
        );
        assert_eq!(
            answer(&runtime, "focus @2"),
            "error no window @2\n",
            "a window that is gone was called floating"
        );
    }

    #[test]
    fn the_layout_disagreeing_with_itself_is_not_the_callers_fault() {
        // A workspace outside 1-9 cannot cross the wire — `workspace` bounds
        // it while parsing — so reaching the layout's refusal means the
        // arrangement contradicted the `is_arranged` it just answered. That
        // is `unavailable`: telling a caller to fix a request they could not
        // have sent is the wrong instruction.
        let window = SurfaceKey {
            client: 1,
            object: 1,
        };
        let (runtime, _frame) = session_of(&[window]);
        let named = handle_of(&layout_of(&runtime), window);
        let refused = apply(
            &mut runtime.lock().expect("runtime"),
            Request::SendWindow(named, 42),
        )
        .expect_err("an impossible workspace was accepted");
        assert!(
            refused.contains("layout refused"),
            "refused for the wrong reason: {refused}"
        );
        // And the parser refuses the same number before the runtime ever sees
        // it. Spelled `@1`, because `send 1:1 42` would now be refused for its
        // FIRST argument and prove nothing about the second.
        let refused = Request::parse("send @1 42").expect_err("42 parsed");
        assert!(
            refused.contains("workspace"),
            "an out-of-range workspace was refused for the wrong reason: \
             {refused}"
        );
    }

    #[test]
    fn an_overloaded_verb_tells_its_forms_apart_by_shape() {
        // An id begins `@` and nothing else in this grammar does, so the two
        // forms need no argument counting. A word beginning `@` that is not an
        // id is an error rather than a fall-through: `send @x 3` has to
        // complain about the id and not about a workspace number.
        //
        // Counting would not even be available for `send`, which is the point
        // of the sigil: `send 7` is a complete request already, so a bare
        // number could not be told from a workspace without guessing which
        // the caller meant.
        assert_eq!(Request::parse("focus @5"), Ok(Request::FocusWindow(5)));
        assert_eq!(Request::parse("focus left"), Ok(Request::Focus(Direction::Left)));
        assert_eq!(Request::parse("send 3"), Ok(Request::Send(3)));
        assert_eq!(
            Request::parse("send @5 3"),
            Ok(Request::SendWindow(5, 3))
        );
        assert_eq!(Request::parse("move up"), Ok(Request::Move(Direction::Up)));
        assert_eq!(
            Request::parse("move @5 up"),
            Ok(Request::MoveWindow(
                5,
                Direction::Up
            ))
        );

        // A bare number stays the workspace form, so the two cannot collide.
        assert_eq!(Request::parse("send 7"), Ok(Request::Send(7)));

        // `@@5` is in the list because one sigil is the whole rule: a caller
        // building an id by pasting `@` onto a number already read from a
        // report would otherwise silently address window 5.
        for wrong in [
            "send @x 3",
            "focus @",
            "focus @5:2",
            "focus @@5",
            "move @1@2 left",
        ] {
            let refused = Request::parse(wrong).expect_err("a bad id parsed");
            assert!(
                refused.contains("window id"),
                "'{wrong}' was refused for the wrong reason: {refused}"
            );
        }
        // A named form still refuses a bad SECOND argument AS the second
        // argument. These read `@5` and not `1:5` on purpose: spelled the old
        // way they are refused for their first word, and would pass while
        // saying nothing about the workspace or direction they claim to pin.
        let refused = Request::parse("send @5 99").expect_err("99 parsed");
        assert!(
            refused.contains("workspace"),
            "a bad workspace was refused for the wrong reason: {refused}"
        );
        let refused = Request::parse("move @5 sideways").expect_err("parsed");
        assert!(
            refused.contains("direction"),
            "a bad direction was refused for the wrong reason: {refused}"
        );
        // And the trailing-argument rule still holds over the longer forms.
        assert!(Request::parse("send @5 3 4").is_err());

        // An `@` that is not the FIRST character is not an attempt to name a
        // window. `contains` here instead of `starts_with` would swallow the
        // word and answer with an id complaint about an argument whose actual
        // problem is that it is not a direction.
        let refused = Request::parse("move 1@2 left").expect_err("parsed");
        assert!(
            refused.contains("direction"),
            "a malformed direction was read as an id: {refused}"
        );
        let refused = Request::parse("send 1@2").expect_err("parsed");
        assert!(
            refused.contains("workspace"),
            "a malformed workspace was read as an id: {refused}"
        );
    }

    #[test]
    fn the_address_this_channel_used_to_take_says_so() {
        // `client:object` was the address one increment ago. A caller working
        // from an old note or an old report gets told the FORM is retired,
        // rather than told its workspace number is unreadable — which would
        // send it to correct the one word in the line that is right.
        for wrong in ["send 1:5 3", "focus 1:5", "move 1:5 left", "send 12:7"] {
            let refused = Request::parse(wrong).expect_err("a retired id parsed");
            assert!(
                refused.contains("retired") && refused.contains("@12"),
                "'{wrong}' was refused for the wrong reason: {refused}"
            );
        }
        // Only the shape that WAS an address, and only where one could have
        // been written. Everything below keeps its OWN complaint: a word that
        // never looked like an address, a half of one, and — the inversion
        // this is most easily got wrong by — a second argument, where the
        // caller spelled the window correctly and fumbled the other word.
        for (wrong, expected) in [
            ("send a:b", "workspace"),
            ("focus :5", "direction"),
            ("send 1: 3", "workspace"),
            ("workspace 1:5", "workspace"),
            ("send @1 1:5", "workspace"),
            ("move @1 1:5", "direction"),
        ] {
            let refused = Request::parse(wrong).expect_err("parsed");
            assert!(
                refused.contains(expected) && !refused.contains("retired"),
                "'{wrong}' should complain about the {expected}: {refused}"
            );
        }
    }

    #[test]
    fn an_id_is_digits_and_nothing_a_number_parser_would_forgive() {
        // `u64::from_str` accepts a leading `+` and any number of leading
        // zeros, so `@+5` and `@005` would both be window 5 without this —
        // a second and third spelling of one address, arrived at silently.
        // That is the same failure `@@5` is refused for, and worse: a caller
        // pasting `@` onto a signed number would never learn it had been
        // forgiven, and two callers would disagree about what they addressed.
        for wrong in ["focus @+5", "focus @005", "focus @0", "focus @-1"] {
            let refused = Request::parse(wrong).expect_err("a lax id parsed");
            assert!(
                refused.contains("is not a window id"),
                "'{wrong}' was refused for the wrong reason: {refused}"
            );
        }
        // And the one spelling that is an id still is, at both ends of the
        // range a report can print.
        assert_eq!(Request::parse("focus @5"), Ok(Request::FocusWindow(5)));
        assert_eq!(Request::parse("focus @1"), Ok(Request::FocusWindow(1)));
        assert_eq!(
            Request::parse("focus @10"),
            Ok(Request::FocusWindow(10)),
            "a zero that is not the LEADING digit is an ordinary digit"
        );
    }

    #[test]
    fn an_app_id_is_one_word_however_the_client_spells_it() {
        // Every field before `title` has to be a single token, because only
        // the last one may run to the end of the line. A client picks this
        // string, so a space in it would put the fields after it somewhere no
        // reader expects — the same objection `clean_title` answers, needing
        // a different answer because the position is different.
        assert_eq!(clean_app_id(Some("org.mozilla.firefox")), "org.mozilla.firefox");
        assert_eq!(clean_app_id(None), "", "absent must be an empty value");
        assert_eq!(clean_app_id(Some("two words")), "two_words");
        assert_eq!(clean_app_id(Some("a\tb\nc")), "a_b_c");
        // The characters `reportable` refuses go the same way, for its
        // reasons: a paragraph separator ends a line for readers that are not
        // `str::lines`, and a bidi override changes how a record READS.
        assert_eq!(clean_app_id(Some("a\u{2029}b")), "a_b");
        assert_eq!(clean_app_id(Some("a\u{202e}b")), "a_b");
        // Ordinary text in other scripts survives, as it must.
        assert_eq!(clean_app_id(Some("организация.браузер")), "организация.браузер");
        // And it is bounded, like the title.
        assert_eq!(clean_app_id(Some(&"x".repeat(500))).chars().count(), TITLE_LIMIT);
    }

    #[test]
    fn an_app_id_cannot_steer_a_reader_to_the_wrong_title() {
        // An app id may contain `=` — nothing in the grammar forbids it, and
        // forbidding it would mangle an ordinary value for no gain. So a
        // client can put the text `title=` in its OWN field. A reader that
        // hunts for the first `title=` anywhere in the line finds the
        // client's; one that takes `" title="`, the way the record is
        // written, does not. The second field a client chooses is what makes
        // this reachable, so it is pinned rather than left to a comment.
        let window = SurfaceKey {
            client: 1,
            object: 1,
        };
        let (runtime, _frame) = session_of(&[window]);
        {
            let mut locked = runtime.lock().expect("runtime");
            locked
                .set_application_id(window, "x title=forged")
                .expect("app id");
            locked.set_title(window, "real".to_string()).expect("title");
        }
        let listed = layout_of(&runtime);
        let record = record_for(&listed, handle_of(&listed, window));
        let honest = record
            .split_once(" title=")
            .map(|(_, title)| title)
            .expect("title field");
        assert_eq!(honest, "real", "the app id captured the title: {record}");
        // The forged text survives in the field the client owns, as it must:
        // a client may SAY anything, it may not become another field.
        assert!(record.contains("app_id=x_title=forged "), "{record}");
    }

    #[test]
    fn the_report_says_how_many_records_follow_it() {
        // §15 named a limit it did not close: a body abandoned BETWEEN
        // records reads as a complete, shorter report. The terminator check
        // catches a body that stops mid-record; this is what catches one that
        // stops cleanly at a record boundary, which is the case that was left
        // standing — including the degenerate one, an `ok` with no records at
        // all where there should have been some.
        let first = SurfaceKey {
            client: 1,
            object: 1,
        };
        let second = SurfaceKey {
            client: 2,
            object: 2,
        };
        let (runtime, _frame) = session_of(&[first, second]);
        let report = match apply(
            &mut runtime.lock().expect("runtime"),
            Request::Layout,
        )
        .expect("layout")
        {
            Answer::Report(report) => report,
            _ => panic!("layout did not answer with a report"),
        };
        assert!(
            report.starts_with("output width=240 height=600 windows=2\n"),
            "the count does not lead the report:\n{report}"
        );
        assert_eq!(
            windows_in(&report).len(),
            2,
            "the count and the records disagree:\n{report}"
        );
    }

    #[test]
    fn the_report_names_a_window_by_what_its_client_calls_itself() {
        // An id is stable while a window lives but is a Wayland object number:
        // it means nothing to a person and differs every boot. This is the
        // field an agent told to move "the browser" can actually match on.
        let window = SurfaceKey {
            client: 1,
            object: 1,
        };
        let (runtime, _frame) = session_of(&[window]);
        {
            let mut runtime = runtime.lock().expect("runtime");
            runtime
                .set_application_id(window, "org.mozilla.firefox")
                .expect("app id");
        }
        let report = match apply(
            &mut runtime.lock().expect("runtime"),
            Request::Layout,
        )
        .expect("layout")
        {
            Answer::Report(report) => report,
            _ => panic!("layout did not answer with a report"),
        };
        assert!(
            record_for(&report, handle_of(&report, window)).contains("app_id=org.mozilla.firefox "),
            "the report does not carry the client's own name:\n{report}"
        );
        // `title` stays last, so the app id has to sit before it and the
        // title's run-to-end-of-line rule is untouched.
        let record = record_for(&report, handle_of(&report, window));
        let app_at = record.find(" app_id=").expect("app id field");
        let title_at = record.find(" title=").expect("title field");
        assert!(app_at < title_at, "app_id must precede title: {record}");
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
            report.contains("window id=@1 object=1:1 workspace=1 "),
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
        assert!(report.contains("window id=@1 object=1:1 workspace=1 "), "{report}");
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
            report.contains("window id=@1 object=1:1 workspace=3 "),
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
            report.contains("window id=@1 object=1:1 workspace=3 ") && report.contains("visible=false"),
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
            split_answer(
                "ok\noutput width=1 height=2 windows=1\nworkspace active=1 \
                 occupied=1\nwindow id=@1 object=1:1 works",
                true
            ),
            Err(ControlFailure::Unreachable(_))
        ));
        let empty = "output width=1 height=2 windows=0\nworkspace active=1 \
                     occupied=\n";
        assert_eq!(
            split_answer(&format!("ok\n{empty}"), true),
            Ok(empty.to_string())
        );

        // The truncation the terminator cannot see: whole records, just not
        // all of them. Nothing about this body's SHAPE is wrong, which is why
        // the sender's own count is the only thing that can say so.
        let short = "ok\noutput width=1 height=2 windows=2\n\
                     workspace active=1 occupied=1\n\
                     window id=@1 object=1:1 workspace=1 x=0 y=0 width=1 \
                     height=1 \
                     visible=true focused=true fullscreen=false floating=false \
                     parent= app_id= title=\n";
        assert!(
            matches!(
                split_answer(short, true),
                Err(ControlFailure::Unreachable(_))
            ),
            "a report that lost a whole record read as a shorter one"
        );
        // And the degenerate case, an `ok` abandoned before its first record.
        assert!(matches!(
            split_answer(
                "ok\noutput width=1 height=2 windows=3\nworkspace active=1 \
                 occupied=1\n",
                true
            ),
            Err(ControlFailure::Unreachable(_))
        ));
        // A body that declares nothing is not a report this client knows.
        assert!(matches!(
            split_answer("ok\nsomething else entirely\n", true),
            Err(ControlFailure::Unreachable(_))
        ));
    }

    #[test]
    fn a_report_cut_to_its_status_line_is_not_an_answered_question() {
        // The emptiest truncation there is, and the one the count could not
        // see on its own: a `layout` whose body never arrived is `ok\n`, which
        // is exactly what a successful ORDER answers. Judged without knowing
        // which was asked, it read as a working compositor with nothing on
        // screen — so the check is told what the request was.
        assert!(
            matches!(
                split_answer("ok\n", true),
                Err(ControlFailure::Unreachable(_))
            ),
            "a question answered with no body reported success"
        );
        assert_eq!(split_answer("ok\n", false), Ok(String::new()));
        // And the other direction: an order does not answer with a report, so
        // a body here is an answer this client does not understand.
        assert!(matches!(
            split_answer("ok\noutput width=1 height=2 windows=0\n", false),
            Err(ControlFailure::Unreachable(_))
        ));

        // A report owes its workspace line as well. It is derived from the
        // same arrangement the windows are, so a body carrying the output line
        // and the right window count but nothing from the bar is not a shorter
        // report — it is one that lost a record the count cannot count.
        assert!(
            matches!(
                split_answer("ok\noutput width=1 height=2 windows=0\n", true),
                Err(ControlFailure::Unreachable(_))
            ),
            "a report with no workspace line read as complete"
        );
        // Order matters too: the output line is what carries the count, so a
        // body that does not begin with it has nothing to check against.
        assert!(matches!(
            split_answer(
                "ok\nworkspace active=1 occupied=1\noutput width=1 height=2 \
                 windows=0\n",
                true
            ),
            Err(ControlFailure::Unreachable(_))
        ));
        // And the workspace line is owed in its PLACE, not merely somewhere:
        // asked with `any`, a report that carried it after the windows passed,
        // and the fields of a record are only findable because every record is
        // where the format says it is.
        assert!(
            matches!(
                split_answer(
                    "ok\noutput width=1 height=2 windows=1\n\
                     window id=@1 object=1:1 workspace=1 x=0 y=0 width=1 \
                     height=1 \
                     visible=true focused=true fullscreen=false floating=false \
                     parent= app_id= title=\n\
                     workspace active=1 occupied=1\n",
                    true
                ),
                Err(ControlFailure::Unreachable(_))
            ),
            "a report whose records are out of order read as complete"
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
            split_answer("unavailable compositor runtime is poisoned\n", true),
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
            split_answer("ok", true),
            Err(ControlFailure::Unreachable(_))
        ));
        assert!(matches!(
            split_answer("error truncated", true),
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
        let mut found: Vec<String> = Vec::new();
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
                found.push(verb.to_string());
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
        // SETS, not counts. A verb may carry more than one form — `focus`
        // takes a direction or a window id — so `USAGE` has more entries than
        // the parser has arms, and comparing lengths would either forbid the
        // overload or, with a count fudged to allow it, stop noticing a verb
        // documented and never implemented. The two directions are asked
        // separately above and here.
        let documented: BTreeSet<&str> = USAGE
            .iter()
            .filter_map(|(form, _)| form.split_whitespace().next())
            .collect();
        let taken: BTreeSet<&str> = found.iter().map(String::as_str).collect();
        assert_eq!(
            taken, documented,
            "the parser and USAGE no longer name the same verbs"
        );
    }

    #[test]
    fn the_client_tells_a_refusal_from_an_empty_room() {
        // Two statuses, because a typo and a session that is not running are
        // different things to have to fix.
        assert_eq!(split_answer("ok\n", false), Ok(String::new()));
        let empty = "output width=1 height=2 windows=0\nworkspace active=1 \
                     occupied=\n";
        assert_eq!(
            split_answer(&format!("ok\n{empty}"), true),
            Ok(empty.to_string())
        );
        assert_eq!(
            split_answer("error no such request 'dance'\n", false),
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
            split_answer("hello\n", false),
            Err(ControlFailure::Unreachable(_))
        ));
        assert!(matches!(
            split_answer("", false),
            Err(ControlFailure::Unreachable(_))
        ));
    }
}
