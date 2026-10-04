//! The pinentry side of gpg-agent's pinentry protocol, an Assuan
//! conversation over standard input and output: one command a line, each
//! answered by `OK` or `ERR`, a passphrase returned in `D` lines. The
//! commands set the texts of the next prompt; `GETPIN`, `CONFIRM` and
//! `MESSAGE` show it and wait for the person.

use std::collections::VecDeque;
use std::io::{self, BufRead, Write};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use crate::request::{wipe, Answer, Kind, Request, DEFAULT_REPEAT_ERROR};

/// The longest line this side sends, its newline included.
pub const MAX_LINE: usize = 1000;

/// The longest line read, its CR and LF included: libassuan's
/// `ASSUAN_LINELENGTH`, which a peer may fill.
pub const MAX_READ: usize = 1002;

/// Lines the reader may hold before it waits for the conversation, so a
/// caller that floods standard input holds this much and no more.
const QUEUE: usize = 16;

/// libgpg-error codes as pinentry sends them: its source, 5, in the top
/// byte.
const SOURCE: u32 = 5 << 24;
pub const CANCELED: u32 = SOURCE | 99;
pub const NOT_CONFIRMED: u32 = SOURCE | 114;
pub const TIMEOUT: u32 = SOURCE | 62;
pub const NO_PIN_ENTRY: u32 = SOURCE | 85;
pub const UNKNOWN_OPTION: u32 = SOURCE | 174;
pub const LINE_TOO_LONG: u32 = SOURCE | 263;
pub const UNKNOWN_COMMAND: u32 = SOURCE | 275;
pub const PARAMETER: u32 = SOURCE | 280;

/// What the reader hands the conversation.
#[derive(Debug, PartialEq)]
pub enum Event {
    /// One line, its newline removed.
    Line(Vec<u8>),
    /// A line longer than `MAX_READ`, read through and dropped.
    TooLong,
    /// Standard input ended or failed: the caller has gone.
    End,
}

/// Reads `input` a line at a time on a thread of its own, so the window
/// can notice the caller hanging up while it waits for the person.
pub fn spawn_reader(input: impl io::Read + Send + 'static) -> Result<Receiver<Event>, String> {
    let (lines, receiver) = mpsc::sync_channel(QUEUE);
    std::thread::Builder::new()
        .name("assuan-reader".to_owned())
        .spawn(move || {
            let mut input = io::BufReader::new(input);
            loop {
                let event = match read_line(&mut input) {
                    Ok(Some(event)) => event,
                    Ok(None) | Err(_) => Event::End,
                };
                let end = event == Event::End;
                if lines.send(event).is_err() || end {
                    break;
                }
            }
        })
        .map_err(|error| format!("cannot start the protocol reader: {error}"))?;
    Ok(receiver)
}

/// The next line of `input`: `None` at its end, a line cut short by the
/// end included, since the protocol ends every line. A read a signal
/// interrupted is read again.
fn read_line(input: &mut impl BufRead) -> io::Result<Option<Event>> {
    let mut line = Vec::new();
    let mut over = false;
    loop {
        let buffer = match input.fill_buf() {
            Ok(buffer) => buffer,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if buffer.is_empty() {
            return Ok(None);
        }
        let (taken, ended) = match buffer.iter().position(|byte| *byte == b'\n') {
            Some(at) => (at + 1, true),
            None => (buffer.len(), false),
        };
        if !over {
            match buffer.get(..taken) {
                Some(part) if line.len() + part.len() <= MAX_READ => line.extend_from_slice(part),
                _ => {
                    over = true;
                    line.clear();
                }
            }
        }
        input.consume(taken);
        if ended {
            if over {
                return Ok(Some(Event::TooLong));
            }
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(Event::Line(line)));
        }
    }
}

/// The reader's lines, with a way to look for the caller's end without
/// waiting and without losing a line that arrived first.
pub struct Inbox {
    receiver: Receiver<Event>,
    pending: VecDeque<Event>,
    ended: bool,
}

impl Inbox {
    pub fn new(receiver: Receiver<Event>) -> Self {
        Self {
            receiver,
            pending: VecDeque::new(),
            ended: false,
        }
    }

    /// The next event, waiting for it.
    fn next(&mut self) -> Event {
        if let Some(event) = self.pending.pop_front() {
            return event;
        }
        if self.ended {
            return Event::End;
        }
        self.receiver.recv().unwrap_or(Event::End)
    }

    /// Whether the caller has gone, without waiting. Lines that arrive
    /// meanwhile keep their turn. An agent sends nothing while it waits for
    /// an answer, so a caller that fills the reader's queue is taken as
    /// gone: its end could not be seen behind the lines.
    pub fn hung_up(&mut self) -> bool {
        while !self.ended {
            if self.pending.len() >= QUEUE {
                self.ended = true;
                break;
            }
            match self.receiver.try_recv() {
                Ok(Event::End) | Err(TryRecvError::Disconnected) => self.ended = true,
                Ok(event) => self.pending.push_back(event),
                Err(TryRecvError::Empty) => break,
            }
        }
        self.ended
    }
}

/// The texts the agent set for the prompts that follow.
#[derive(Default)]
struct Settings {
    title: Option<String>,
    description: Option<String>,
    prompt: Option<String>,
    error: Option<String>,
    ok: Option<String>,
    cancel: Option<String>,
    not_ok: Option<String>,
    repeat: Option<String>,
    repeat_error: Option<String>,
    timeout: Option<u64>,
}

/// The labels `OPTION default-*` gives, used where no `SET*` names one;
/// a `RESET` keeps them, as it keeps every option and the timeout.
#[derive(Default)]
struct Defaults {
    ok: Option<String>,
    cancel: Option<String>,
    prompt: Option<String>,
}

#[derive(Default)]
struct Session {
    settings: Settings,
    defaults: Defaults,
}

/// Whether the conversation goes on.
enum Next {
    Continue,
    Stop,
}

impl Session {
    fn request(&self, kind: Kind) -> Request {
        let mut request = Request::new(kind);
        let settings = &self.settings;
        let pick = |set: &Option<String>, default: &Option<String>| {
            set.as_deref()
                .or(default.as_deref())
                .filter(|text| !text.is_empty())
                .map(label)
        };
        if let Some(title) = &settings.title {
            request.title = title.clone();
        }
        request.description = settings.description.clone().unwrap_or_default();
        if let Some(prompt) = pick(&settings.prompt, &self.defaults.prompt) {
            request.prompt = prompt;
        }
        request.error = settings.error.clone().filter(|error| !error.is_empty());
        if let Some(ok) = pick(&settings.ok, &self.defaults.ok) {
            request.ok = ok;
        }
        if let Some(cancel) = pick(&settings.cancel, &self.defaults.cancel) {
            request.cancel = cancel;
        }
        if request.kind == (Kind::Confirm { one_button: false }) {
            request.not_ok = pick(&settings.not_ok, &None);
        }
        request.timeout = settings.timeout;
        request
    }

    /// One command line, answered on `out`.
    fn command(
        &mut self,
        line: &[u8],
        inbox: &mut Inbox,
        out: &mut dyn Write,
        ask: &mut dyn FnMut(Request, &mut Inbox) -> Answer,
    ) -> io::Result<Next> {
        if line.is_empty() || line.first() == Some(&b'#') {
            return Ok(Next::Continue);
        }
        // The command ends at a space or tab, and its parameters start at
        // the first byte after that is neither, as libassuan reads them.
        let blank = |byte: &u8| matches!(byte, b' ' | b'\t');
        let (word, raw) = match line.iter().position(blank) {
            Some(at) => {
                let rest = line.get(at..).unwrap_or_default();
                let start = rest.iter().position(|byte| !blank(byte));
                let raw = start.and_then(|start| rest.get(start..));
                (line.get(..at).unwrap_or_default(), raw.unwrap_or_default())
            }
            None => (line, &[][..]),
        };
        let word = String::from_utf8_lossy(word).to_ascii_uppercase();
        let text = || unescape(raw);
        let settings = &mut self.settings;
        match word.as_str() {
            "NOP" | "HELP" | "SETREPEATOK" | "SETQUALITYBAR" | "SETQUALITYBAR_TT" | "SETGENPIN"
            | "SETGENPIN_TT" | "SETKEYINFO" | "CLEARPASSPHRASE" => ok(out)?,
            "OPTION" => {
                if self.option(&text()) {
                    ok(out)?;
                } else {
                    error(out, UNKNOWN_OPTION, "Unknown option")?;
                }
            }
            "GETINFO" => match text().trim() {
                "flavor" => data_ok(out, b"td")?,
                "version" => data_ok(out, env!("CARGO_PKG_VERSION").as_bytes())?,
                "pid" => data_ok(out, std::process::id().to_string().as_bytes())?,
                "ttyinfo" => data_ok(out, b"- - -")?,
                _ => error(out, PARAMETER, "Invalid parameter")?,
            },
            "SETTITLE" => set(out, &mut settings.title, text())?,
            "SETDESC" => set(out, &mut settings.description, text())?,
            "SETPROMPT" => set(out, &mut settings.prompt, text())?,
            "SETERROR" => set(out, &mut settings.error, text())?,
            "SETOK" => set(out, &mut settings.ok, text())?,
            "SETCANCEL" => set(out, &mut settings.cancel, text())?,
            "SETNOTOK" => set(out, &mut settings.not_ok, text())?,
            "SETREPEATERROR" => set(out, &mut settings.repeat_error, text())?,
            "SETREPEAT" => {
                let label = text();
                let label = if label.is_empty() {
                    "Repeat:".to_owned()
                } else {
                    label
                };
                set(out, &mut settings.repeat, label)?;
            }
            "SETTIMEOUT" => match text().trim().parse::<u64>() {
                Ok(seconds) => {
                    settings.timeout = (seconds > 0).then_some(seconds);
                    ok(out)?;
                }
                Err(_) => error(out, PARAMETER, "Invalid parameter")?,
            },
            "RESET" => {
                self.settings = Settings {
                    timeout: settings.timeout,
                    ..Settings::default()
                };
                ok(out)?;
            }
            "BYE" => {
                out.write_all(b"OK closing connection\n")?;
                out.flush()?;
                return Ok(Next::Stop);
            }
            "GETPIN" => {
                let repeated = settings.repeat.is_some();
                let kind = Kind::Text {
                    masked: true,
                    repeat: settings.repeat.take().map(|repeat| label(&repeat)),
                    repeat_error: settings
                        .repeat_error
                        .clone()
                        .unwrap_or_else(|| DEFAULT_REPEAT_ERROR.to_owned()),
                };
                let request = self.request(kind);
                self.settings.error = None;
                return answer(out, ask(request, inbox), repeated);
            }
            "CONFIRM" | "MESSAGE" => {
                let one_button =
                    word == "MESSAGE" || text().split_whitespace().any(|arg| arg == "--one-button");
                let request = self.request(Kind::Confirm { one_button });
                self.settings.error = None;
                return answer(out, ask(request, inbox), false);
            }
            _ => error(out, UNKNOWN_COMMAND, "Unknown IPC command")?,
        }
        Ok(Next::Continue)
    }

    /// Takes an option, or answers false for one this program does not
    /// implement, so the agent never counts on a feature it lacks (an
    /// external password cache, enforced constraints, a formatted
    /// passphrase).
    fn option(&mut self, option: &str) -> bool {
        let (name, value) = match option.split_once(['=', ' ']) {
            Some((name, value)) => (name.trim(), value.trim()),
            None => (option.trim(), ""),
        };
        let value = (!value.is_empty()).then(|| value.to_owned());
        match name {
            "default-ok" => self.defaults.ok = value,
            "default-cancel" => self.defaults.cancel = value,
            "default-prompt" => self.defaults.prompt = value,
            // These describe a terminal, a toolkit or labels this window
            // does not use; taking them changes nothing.
            "display" | "ttyname" | "ttytype" | "lc-ctype" | "lc-messages" | "owner"
            | "parent-wid" | "grab" | "no-grab" | "touch-file" | "invisible-char"
            | "debug-wait" => {}
            name if name.starts_with("default-") => {}
            _ => return false,
        }
        true
    }
}

/// Serves one conversation: the greeting, then each command until `BYE`
/// or the caller's end. `ask` shows a prompt and waits for its answer,
/// watching `inbox` for the caller hanging up meanwhile.
pub fn serve(
    inbox: &mut Inbox,
    out: &mut dyn Write,
    ask: &mut dyn FnMut(Request, &mut Inbox) -> Answer,
) -> io::Result<()> {
    out.write_all(b"OK Pleased to meet you\n")?;
    out.flush()?;
    let mut session = Session::default();
    loop {
        let line = match inbox.next() {
            Event::End => return Ok(()),
            Event::TooLong => {
                error(out, LINE_TOO_LONG, "Line too long")?;
                continue;
            }
            Event::Line(line) => line,
        };
        if let Next::Stop = session.command(&line, inbox, out, ask)? {
            return Ok(());
        }
    }
}

/// A prompt's answer on the wire. A passphrase typed twice is reported
/// as such first, as pinentry does.
fn answer(out: &mut dyn Write, answer: Answer, repeated: bool) -> io::Result<Next> {
    match answer {
        Answer::Text(secret) => {
            if repeated {
                out.write_all(b"S PIN_REPEATED\n")?;
            }
            data(out, secret.as_str().as_bytes())?;
            ok(out)?;
        }
        Answer::Confirmed => ok(out)?,
        Answer::Cancelled => error(out, CANCELED, "Operation cancelled")?,
        Answer::Declined => error(out, NOT_CONFIRMED, "Not confirmed")?,
        Answer::TimedOut => error(out, TIMEOUT, "Timeout")?,
        Answer::Hangup => return Ok(Next::Stop),
        Answer::Failed(why) => {
            eprintln!("td-pinentry: {why}");
            error(out, NO_PIN_ENTRY, "No pinentry")?;
        }
    }
    Ok(Next::Continue)
}

fn ok(out: &mut dyn Write) -> io::Result<()> {
    out.write_all(b"OK\n")?;
    out.flush()
}

fn error(out: &mut dyn Write, code: u32, text: &str) -> io::Result<()> {
    writeln!(out, "ERR {code} {text}")?;
    out.flush()
}

fn set(out: &mut dyn Write, slot: &mut Option<String>, text: String) -> io::Result<()> {
    *slot = Some(text);
    ok(out)
}

fn data_ok(out: &mut dyn Write, bytes: &[u8]) -> io::Result<()> {
    data(out, bytes)?;
    ok(out)
}

/// `bytes` as `D` lines of at most `MAX_LINE` bytes each, `%`, CR and LF
/// escaped and no escape split across lines; nothing for no bytes. The
/// line buffer is zeroed after, since it may hold a passphrase.
fn data(out: &mut dyn Write, bytes: &[u8]) -> io::Result<()> {
    let mut line = Vec::new();
    line.try_reserve_exact(MAX_LINE)
        .map_err(|_| io::Error::other("cannot reserve the data line"))?;
    let written = write_data(out, bytes, &mut line);
    wipe(line);
    written
}

fn write_data(out: &mut dyn Write, bytes: &[u8], line: &mut Vec<u8>) -> io::Result<()> {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut pending = false;
    for byte in bytes.iter().copied() {
        let escaped = matches!(byte, b'%' | b'\r' | b'\n');
        let width = if escaped { 3 } else { 1 };
        if !pending {
            line.extend_from_slice(b"D ");
            pending = true;
        }
        if line.len() + width + 1 > MAX_LINE {
            line.push(b'\n');
            out.write_all(line)?;
            line.fill(0);
            line.clear();
            line.extend_from_slice(b"D ");
        }
        if escaped {
            let high = HEX.get(usize::from(byte >> 4)).copied().unwrap_or(b'0');
            let low = HEX.get(usize::from(byte & 15)).copied().unwrap_or(b'0');
            line.extend_from_slice(&[b'%', high, low]);
        } else {
            line.push(byte);
        }
    }
    if pending {
        line.push(b'\n');
        out.write_all(line)?;
    }
    Ok(())
}

/// A parameter with its `%XX` escapes decoded, read as UTF-8 with any
/// invalid sequence replaced. A `%` without two hex digits stays as it is.
pub fn unescape(raw: &[u8]) -> String {
    let mut bytes = Vec::with_capacity(raw.len());
    let mut rest = raw;
    while let Some((&byte, tail)) = rest.split_first() {
        rest = tail;
        if byte == b'%' {
            if let [high, low, after @ ..] = rest {
                if let (Some(high), Some(low)) = (hex(*high), hex(*low)) {
                    bytes.push(high << 4 | low);
                    rest = after;
                    continue;
                }
            }
        }
        bytes.push(byte);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn hex(digit: u8) -> Option<u8> {
    char::from(digit)
        .to_digit(16)
        .and_then(|value| u8::try_from(value).ok())
}

/// A button or field label without its mnemonic: GTK's `_` before the
/// access key dropped, `__` kept as one `_`.
pub fn label(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '_' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('_') => out.push('_'),
            Some(next) => out.push(next),
            None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests;
