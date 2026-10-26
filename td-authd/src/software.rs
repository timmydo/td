//! td-software's application requests, root's serving half (td-authd/
//! DESIGN.md, "Application requests"): request `24` runs the root
//! `application-cache` operation for one validated name as a process of
//! its own, never waiting on it, and answers at once. A status answers
//! with what the last read found while the next read runs.

use crate::application_cache::Verb;
use crate::flathub_app::{self, Status};
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The names one generation keeps at once; one idle and unnamed for a
/// status interval is forgotten to make room for another, so cycling names
/// starts no more reads than the interval allows each kept one.
const NAMES: usize = 16;
/// The soonest a status read for a name starts again after the last
/// began, unless an operation on it ended since: each read walks the
/// generations' deploys (td-authd/DESIGN.md, "Application cache
/// operation").
const STATUS_INTERVAL: Duration = Duration::from_secs(2);
/// How recent a read must be for an operation to start on what it found:
/// td-software asks a status at least this often.
const FRESH: Duration = Duration::from_secs(30);
/// How long an operation that has exited may hold its output open in
/// another process before root takes what it read as all of it.
const EXIT_GRACE: Duration = Duration::from_secs(2);
/// Past the operation's own bound, root stops waiting for it. The
/// operation kills td-app at its bound and exits, so this is reached only
/// by an operation that cannot.
const BACKSTOP: Duration = Duration::from_secs(60);
/// The longest refusal reason an answer carries.
pub(crate) const REASON_BYTES: usize = 200;
/// The diagnostics' tail root keeps while an operation runs.
const TAIL_BYTES: usize = 512;
/// What one advance reads of a child at most, so a request's answer is
/// never held by a child writing.
const READ_BUDGET: usize = 16 * 1024;
/// A frame's payload bound (td-authd/DESIGN.md, "Private channel").
const FRAME_BYTES: usize = crate::channel::MAX_MESSAGE;
/// The image's application registry (APPLICATIONS.md §A.0), immutable:
/// the names the installed set follows.
pub(crate) const REGISTRY: &str = "/etc/td-applications.tsv";
/// The registry's bounds, the builder's own (engine/src/launcher.rs).
const REGISTRY_BYTES: usize = 1024 * 1024;
const REGISTRY_NAMES: usize = 256;
/// One byte past the bound, so a longer registry reads as too long.
const REGISTRY_READ: u64 = REGISTRY_BYTES as u64 + 1;
/// `88`'s state bits for one Flathub declaration: its last read showed a
/// `current` generation, and a later answer may say otherwise.
const INSTALLED: u8 = 0x01;
const PENDING: u8 = 0x02;

/// One request: a verb and a name in the registry's grammar, which the
/// operation's own admission checks against the deployment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Request {
    pub verb: Verb,
    pub name: String,
}

impl Request {
    /// `24`'s operands: the verb byte and the name's bytes.
    pub(crate) fn decode(operands: &[u8]) -> Result<Self, String> {
        let invalid = || "invalid application request".to_string();
        let [verb, name @ ..] = operands else {
            return Err(invalid());
        };
        let verb = verb_of(*verb).ok_or_else(invalid)?;
        let name = std::str::from_utf8(name).map_err(|_| invalid())?;
        if !crate::consent::application_name(name) {
            return Err(invalid());
        }
        Ok(Self {
            verb,
            name: name.to_string(),
        })
    }
}

fn verb_of(byte: u8) -> Option<Verb> {
    Some(match byte {
        0 => Verb::Status,
        1 => Verb::Install,
        2 => Verb::Update,
        3 => Verb::Remove,
        4 => Verb::DeleteData,
        _ => return None,
    })
}

fn verb_byte(verb: Verb) -> u8 {
    match verb {
        Verb::Status => 0,
        Verb::Install => 1,
        Verb::Update => 2,
        Verb::Remove => 3,
        Verb::DeleteData => 4,
    }
}

/// The fixed root operation for `name`; the name is the only operand a
/// peer chose, and the operation admits it against the deployment.
fn operation(owner: u32, name: &str, verb: Verb) -> Command {
    let mut command = Command::new("/bin/td-authd");
    command.args(["application-cache", &owner.to_string(), name, verb.word()]);
    command
}

/// One operation process, read without blocking: a status's record on its
/// stdout, bounded, or another verb's diagnostics on its stderr, of which
/// the tail is kept.
struct Run {
    child: Child,
    wire: UnixStream,
    bytes: Vec<u8>,
    status: bool,
    eof: bool,
    oversized: bool,
    /// When it was found exited with its output still open.
    exited: Option<Instant>,
    deadline: Instant,
}

enum Ended {
    Running,
    /// Its output, and whether it exited successfully.
    Exited(Vec<u8>, bool),
    /// It outlived root's backstop, or could not be read or reaped.
    Lost,
}

impl Run {
    fn start(mut command: Command, verb: Verb) -> Result<Self, String> {
        let (wire, endpoint) = UnixStream::pair().map_err(|e| e.to_string())?;
        let endpoint = Stdio::from(OwnedFd::from(endpoint));
        let status = verb == Verb::Status;
        let (stdout, stderr) = if status {
            (endpoint, Stdio::null())
        } else {
            (Stdio::null(), endpoint)
        };
        // The temporary Command drops its copy of the endpoint, so EOF
        // comes when the operation exits. Like every authority child it
        // gets null stdin, `/` and no environment: root's fd 0 is its
        // channel to the compositor.
        let child = command
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .map_err(|e| format!("start application request: {e}"))?;
        Self::new(child, wire, verb)
    }

    fn new(mut child: Child, wire: UnixStream, verb: Verb) -> Result<Self, String> {
        let deadline = Instant::now()
            .checked_add(verb.lifetime())
            .and_then(|end| end.checked_add(BACKSTOP));
        let (Some(deadline), Ok(())) = (deadline, wire.set_nonblocking(true)) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("application request setup failed".into());
        };
        Ok(Self {
            child,
            wire,
            bytes: Vec::new(),
            status: verb == Verb::Status,
            eof: false,
            oversized: false,
            exited: None,
            deadline,
        })
    }

    fn receive(&mut self) -> Result<(), String> {
        let mut buffer = [0u8; 4096];
        let mut read = 0;
        while !self.eof && read < READ_BUDGET {
            match self.wire.read(&mut buffer) {
                Ok(0) => self.eof = true,
                Ok(count) => {
                    read += count;
                    self.bytes
                        .extend_from_slice(buffer.get(..count).ok_or("invalid read")?);
                    if self.status {
                        if self.bytes.len() > flathub_app::MAX_STATUS_BYTES {
                            self.oversized = true;
                            self.bytes.clear();
                        }
                    } else {
                        let excess = self.bytes.len().saturating_sub(TAIL_BYTES);
                        self.bytes.drain(..excess);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(())
    }

    /// Never blocks but to reap a child it has just killed, which only
    /// the backstop does: an operation is never killed early.
    fn advance(&mut self) -> Ended {
        // An unreadable output ends the reading, not the operation.
        if self.receive().is_err() {
            self.eof = true;
        }
        let now = Instant::now();
        match self.child.try_wait() {
            Ok(Some(status)) => {
                // Exited, its output perhaps held open by a process it left.
                let exited = *self.exited.get_or_insert(now);
                if !self.eof && now.saturating_duration_since(exited) < EXIT_GRACE {
                    return Ended::Running;
                }
                let whole = self.eof && !self.oversized;
                let output = if self.oversized {
                    Vec::new()
                } else {
                    std::mem::take(&mut self.bytes)
                };
                Ended::Exited(output, status.success() && whole)
            }
            Ok(None) | Err(_) if now < self.deadline => Ended::Running,
            Ok(None) | Err(_) => self.lose(),
        }
    }

    fn lose(&mut self) -> Ended {
        let _ = self.child.kill();
        let _ = self.child.wait();
        Ended::Lost
    }
}

/// What one read of a name's status found besides its record.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Found {
    /// A writer at work in the cache.
    writer: bool,
    /// A `current` generation: the application is installed.
    installed: bool,
}

/// What the last read of a name's status found.
enum Record {
    Unread,
    /// The canonical record, when its read began, and what it showed.
    Read(Vec<u8>, Instant, Found),
    /// A read that failed, and when it began.
    Failed(Instant),
}

struct Entry {
    operation: Option<(Verb, Run)>,
    /// The last operation that ended: its verb, and its refusal if it
    /// failed.
    last: Option<(Verb, Option<String>)>,
    ended: Option<Instant>,
    reading: Option<Run>,
    began: Option<Instant>,
    /// When the last read ended, read or failed: how recent `record` is.
    finished: Option<Instant>,
    record: Record,
    /// When a request last named it, for forgetting the idlest.
    used: Instant,
}

impl Entry {
    fn new() -> Self {
        Self {
            operation: None,
            last: None,
            ended: None,
            reading: None,
            began: None,
            finished: None,
            record: Record::Unread,
            used: Instant::now(),
        }
    }

    fn idle(&self) -> bool {
        self.operation.is_none() && self.reading.is_none()
    }

    fn advance(&mut self, name: &str) {
        if let Some((verb, run)) = &mut self.operation {
            let verb = *verb;
            let refusal = match run.advance() {
                Ended::Running => return self.advance_read(name),
                Ended::Exited(_, true) => None,
                Ended::Exited(tail, false) => Some(reason(&tail)),
                Ended::Lost => Some("the operation did not finish".to_string()),
            };
            if let Some(refusal) = &refusal {
                let _ = writeln!(
                    io::stderr().lock(),
                    "td-authd: application {} {name}: {refusal}",
                    verb.word()
                );
            }
            self.operation = None;
            self.last = Some((verb, refusal));
            self.ended = Some(Instant::now());
        }
        self.advance_read(name);
    }

    fn advance_read(&mut self, name: &str) {
        let Some(run) = &mut self.reading else {
            return;
        };
        let began = self.began.unwrap_or_else(Instant::now);
        self.record = match run.advance() {
            Ended::Running => return,
            Ended::Exited(bytes, true) => match checked(name, &bytes) {
                Some((record, found)) => Record::Read(record, began, found),
                None => Record::Failed(began),
            },
            Ended::Exited(_, false) | Ended::Lost => Record::Failed(began),
        };
        self.reading = None;
        self.finished = Some(Instant::now());
    }

    /// Starts a read unless one runs or the last began within the
    /// interval, an operation having ended since.
    fn read(&mut self, owner: u32, name: &str, start: &Starter) {
        if self.reading.is_some() {
            return;
        }
        let due = match (self.began, self.ended) {
            (None, _) => true,
            (Some(began), Some(ended)) if ended >= began => true,
            (Some(began), _) => began.elapsed() >= STATUS_INTERVAL,
        };
        if !due {
            return;
        }
        let began = Instant::now();
        self.began = Some(began);
        match start(owner, name, Verb::Status) {
            Ok(run) => self.reading = Some(run),
            Err(why) => {
                let _ = writeln!(io::stderr().lock(), "td-authd: {why}");
                self.record = Record::Failed(began);
                self.finished = Some(Instant::now());
            }
        }
    }

    /// Why an operation may not start now: `ff 01` while one runs or the
    /// last read showed a writer root did not start (`0e`'s check, td-jail's
    /// or a previous generation's), whose lock the operation would wait on
    /// until its bound; `ff 05` until a read that ended recently says
    /// neither. A recent read that failed admits one: td-app cannot say
    /// what a damaged cache holds, `remove` is what clears one, and the
    /// operation's own bound covers a wait on a lock it did not see.
    fn refusal(&self) -> Option<u8> {
        if self.operation.is_some() {
            return Some(1);
        }
        let fresh = self
            .finished
            .is_some_and(|finished| finished.elapsed() < FRESH);
        match &self.record {
            Record::Read(_, began, found) if fresh => {
                // Root's own operation that ended since was that writer.
                let since = self.ended.is_some_and(|ended| ended >= *began);
                (found.writer && !since).then_some(1)
            }
            Record::Failed(_) if fresh => None,
            _ => Some(5),
        }
    }

    /// `88`'s state for this name: whether the last read found it
    /// installed, and whether that may change soon: nothing read yet, an
    /// operation running, or the last read, whether it succeeded or
    /// failed, begun before the last operation ended. A failed read begun
    /// after it is settled and not installed.
    fn installed(&self) -> u8 {
        let (installed, older) = match &self.record {
            Record::Unread => (false, true),
            Record::Failed(began) => (false, self.ended.is_some_and(|ended| ended > *began)),
            Record::Read(_, began, found) => (
                found.installed,
                self.ended.is_some_and(|ended| ended > *began),
            ),
        };
        let pending = older || self.operation.is_some();
        (if installed { INSTALLED } else { 0 }) | (if pending { PENDING } else { 0 })
    }

    /// `87`'s answer: the operation running, the last one's outcome, and
    /// the last record.
    fn answer(&self) -> Vec<u8> {
        let mut answer = vec![
            0x87,
            self.operation
                .as_ref()
                .map_or(0, |(verb, _)| verb_byte(*verb)),
        ];
        match &self.last {
            None => answer.extend_from_slice(&[0, 0, 0]),
            Some((verb, None)) => answer.extend_from_slice(&[verb_byte(*verb), 0, 0]),
            Some((verb, Some(refusal))) => {
                // `reason` made it printable ASCII of at most this many
                // bytes; the cut holds the length byte to it regardless.
                let refusal = refusal.as_bytes();
                let refusal = refusal.get(..REASON_BYTES).unwrap_or(refusal);
                let length = u8::try_from(refusal.len()).unwrap_or(0);
                answer.extend_from_slice(&[verb_byte(*verb), 1, length]);
                answer.extend_from_slice(refusal.get(..usize::from(length)).unwrap_or_default());
            }
        }
        match &self.record {
            Record::Unread => answer.push(0),
            Record::Failed(_) => answer.push(3),
            Record::Read(record, began, _) => {
                let older = self.ended.is_some_and(|ended| ended > *began);
                answer.push(if older { 2 } else { 1 });
                answer.extend_from_slice(record);
            }
        }
        answer
    }
}

/// A record root relays: exactly the canonical status of the requested
/// application, and small enough for a frame beside `87`'s header; and
/// whether it shows a writer at work and a `current` generation.
fn checked(name: &str, bytes: &[u8]) -> Option<(Vec<u8>, Found)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let status = Status::parse(text).ok()?;
    let header = 6 + REASON_BYTES;
    let found = Found {
        writer: status
            .cache
            .as_ref()
            .is_some_and(|cache| cache.progress.is_some()),
        installed: status
            .cache
            .as_ref()
            .is_some_and(|cache| cache.current.is_some()),
    };
    (status.name == name && bytes.len() <= FRAME_BYTES.saturating_sub(header))
        .then(|| (bytes.to_vec(), found))
}

/// The names of the registry's text, in the builder's bounds: one
/// `NAME<TAB>PATH` line each, ended by a newline, at most 256 and none
/// twice. Only names in the registry grammar are kept: that is the
/// grammar every request names an application in, and the builder's
/// identity grammar is wider (APPLICATIONS.md §A), so a name outside it
/// is one no request can name.
pub(crate) fn registry_names(text: &str) -> Result<Vec<String>, String> {
    let invalid = || "the application registry is malformed".to_string();
    if text.len() > REGISTRY_BYTES || !(text.is_empty() || text.ends_with('\n')) {
        return Err(invalid());
    }
    let mut names: Vec<String> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for line in text.split_terminator('\n') {
        let (name, path) = line.split_once('\t').ok_or_else(invalid)?;
        if name.is_empty()
            || path.is_empty()
            || line.contains(['\r', '\0'])
            || path.contains('\t')
            || seen.len() >= REGISTRY_NAMES
            || !seen.insert(name)
        {
            return Err(invalid());
        }
        if crate::consent::application_name(name) {
            names.push(name.to_string());
        }
    }
    Ok(names)
}

/// Reads the image's registry, bounded.
pub(crate) fn load_registry() -> Result<Vec<String>, String> {
    let mut text = String::new();
    std::fs::File::open(REGISTRY)
        .and_then(|file| file.take(REGISTRY_READ).read_to_string(&mut text))
        .map_err(|e| format!("read {REGISTRY}: {e}"))?;
    registry_names(&text)
}

/// Whether a registry name is a Flathub declaration td-app keeps a cache
/// for; the image ships every other one.
fn flathub(name: &str) -> bool {
    flathub_app::flathub_source(name).is_some()
}

/// The operation's last diagnostic line as printable ASCII, without the
/// `td-authd: ` every one of its refusals begins with.
fn reason(tail: &[u8]) -> String {
    let text = String::from_utf8_lossy(tail);
    let line = text.trim_end().rsplit('\n').next().unwrap_or_default();
    let line = line.strip_prefix("td-authd: ").unwrap_or(line);
    let reason: String = flathub_app::reason_text(line)
        .chars()
        .take(REASON_BYTES)
        .collect();
    if reason.is_empty() {
        "the operation failed".into()
    } else {
        reason
    }
}

type Starter = fn(u32, &str, Verb) -> Result<Run, String>;
type Registry = fn() -> Result<Vec<String>, String>;

fn start(owner: u32, name: &str, verb: Verb) -> Result<Run, String> {
    Run::start(operation(owner, name, verb), verb)
}

/// Every name's requests in this generation.
pub(crate) struct Requests {
    entries: BTreeMap<String, Entry>,
    start: Starter,
    /// The registry's names, read once a generation: the image is
    /// immutable while it runs.
    registry: Option<Vec<String>>,
    load: Registry,
    flathub: fn(&str) -> bool,
}

impl Requests {
    pub(crate) fn new() -> Self {
        Self::with(start, load_registry, flathub)
    }

    #[cfg(test)]
    fn with_starter(start: Starter) -> Self {
        Self::with(start, || Ok(Vec::new()), flathub)
    }

    fn with(start: Starter, load: Registry, flathub: fn(&str) -> bool) -> Self {
        Self {
            entries: BTreeMap::new(),
            start,
            registry: None,
            load,
            flathub,
        }
    }

    /// The registry's names, read on first use; a failed read is tried
    /// again by the next request.
    pub(crate) fn registry(&mut self) -> Result<&[String], String> {
        if self.registry.is_none() {
            self.registry = Some((self.load)()?);
        }
        Ok(self.registry.as_deref().unwrap_or_default())
    }

    /// Reaps what ended; every request and heartbeat calls it, so no
    /// finished operation is left a zombie.
    pub(crate) fn advance(&mut self) {
        for (name, entry) in &mut self.entries {
            entry.advance(name);
        }
    }

    /// The name's entry, making room by forgetting the idlest idle one
    /// no request named for a status interval.
    fn entry(&mut self, name: &str) -> Option<&mut Entry> {
        if !self.entries.contains_key(name) {
            if self.entries.len() >= NAMES {
                let idlest = self
                    .entries
                    .iter()
                    .filter(|(_, entry)| entry.idle() && entry.used.elapsed() >= STATUS_INTERVAL)
                    .min_by_key(|(_, entry)| entry.used)
                    .map(|(name, _)| name.clone())?;
                self.entries.remove(&idlest);
            }
            self.entries.insert(name.to_string(), Entry::new());
        }
        let entry = self.entries.get_mut(name)?;
        entry.used = Instant::now();
        Some(entry)
    }

    /// `23`'s answer, at once: `88`, a count and, for each of the
    /// registry's Flathub declarations, its state and its name. A name the
    /// answer leaves out the image ships, which is always installed. Each
    /// is followed through its entry, as a status request would be, so a
    /// read starts no more often than a status's.
    pub(crate) fn installed(&mut self, owner: u32) -> Vec<u8> {
        if std::fs::symlink_metadata(crate::launch::LIVE_MEDIUM).is_ok() {
            return vec![0xff, 3];
        }
        let flathub = self.flathub;
        let names: Vec<String> = match self.registry() {
            Ok(names) => names.iter().filter(|name| flathub(name)).cloned().collect(),
            Err(why) => {
                let _ = writeln!(io::stderr().lock(), "td-authd: {why}");
                return vec![0xff, 2];
            }
        };
        // Leaving one out would say the image ships it; a test holds the
        // declarations to half of what root follows.
        if names.len() > NAMES {
            let _ = writeln!(
                io::stderr().lock(),
                "td-authd: the registry holds more Flathub declarations than root follows"
            );
            return vec![0xff, 2];
        }
        let start = self.start;
        let mut answer = vec![0x88, u8::try_from(names.len()).unwrap_or(0)];
        for name in &names {
            // With no room to follow it, root cannot say yet.
            let state = self.entry(name).map_or(PENDING, |entry| {
                entry.advance(name);
                entry.read(owner, name, &start);
                entry.installed()
            });
            answer.push(state);
            answer.push(u8::try_from(name.len()).unwrap_or(0));
            answer.extend_from_slice(name.as_bytes());
        }
        answer
    }

    /// The configured owner's request, answered at once.
    pub(crate) fn answer(&mut self, owner: u32, request: Request) -> Vec<u8> {
        // The live medium's session runs only the installer.
        if std::fs::symlink_metadata(crate::launch::LIVE_MEDIUM).is_ok() {
            return vec![0xff, 3];
        }
        let start = self.start;
        let Some(entry) = self.entry(&request.name) else {
            return vec![0xff, 4];
        };
        entry.advance(&request.name);
        if request.verb == Verb::Status {
            entry.read(owner, &request.name, &start);
            return entry.answer();
        }
        if let Some(refusal) = entry.refusal() {
            // What a later request needs is a newer read.
            entry.read(owner, &request.name, &start);
            return vec![0xff, refusal];
        }
        match start(owner, &request.name, request.verb) {
            Ok(run) => {
                entry.operation = Some((request.verb, run));
                vec![0x86]
            }
            Err(why) => {
                let _ = writeln!(io::stderr().lock(), "td-authd: {why}");
                vec![0xff, 2]
            }
        }
    }
}

#[cfg(test)]
#[path = "../tests/software.rs"]
mod tests;
