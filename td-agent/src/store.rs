//! The conversation store (DESIGN.md §6): `$XDG_STATE_HOME/td-agent/` on
//! a host. Each conversation is a directory named by a random id holding
//! `meta`, `prefix` and an append-only `log` of newline-delimited JSON
//! events with sequence numbers, and a `lock` its conversation process
//! holds exclusively for as long as it runs: that process is the
//! directory's only writer. The window process reads `meta` for its list
//! and keeps its own `window.lock` and split share at the top.
//!
//! Locks are `File::try_lock`, std's `flock(LOCK_EX | LOCK_NB)`: the
//! kernel drops one when its holder exits however it exits, so there is
//! no stale lock to judge, and std opens every file close-on-exec, so a
//! child never inherits one. Files are opened without following a final
//! symbolic link.

use std::fs::{DirBuilder, File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::cost::Tokens;
use crate::json::Json;

/// `O_NOFOLLOW` on x86-64, as td-ui's control socket spells it.
const O_NOFOLLOW: i32 = 0o400000;

/// The longest `meta` read back.
const MAX_META: u64 = 64 * 1024;
/// The longest `prefix` read back.
pub const MAX_PREFIX: u64 = 1024 * 1024;
/// The longest log loaded; a longer one is refused rather than read.
pub const MAX_LOG: u64 = 256 * 1024 * 1024;
/// What a log keeps free past an accepted message, for the records that
/// follow it: its turn's start and finish, an interruption, a notice.
pub const LOG_RESERVE: u64 = 64 * 1024;
/// The longest line in a log, which a longer one is corrupt past.
/// Below the frame bound by the room an event's wrapping in a message
/// takes, so any line a log holds can be sent to the window.
pub const MAX_LINE: usize = crate::frame::MAX_FRAME - 4096;
/// How long a conversation process waits for its directory's lock while
/// an earlier process of the same conversation is still exiting.
pub const LOCK_WAIT: Duration = Duration::from_secs(3);
/// A title's longest form, in characters.
pub const MAX_TITLE: usize = 80;

/// What a conversation is: the orchestrator is always present and pinned
/// first (DESIGN.md §3); every other conversation is one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    Orchestrator,
    Conversation,
}

impl Role {
    pub fn word(self) -> &'static str {
        match self {
            Self::Orchestrator => "orchestrator",
            Self::Conversation => "conversation",
        }
    }
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "orchestrator" => Some(Self::Orchestrator),
            "conversation" => Some(Self::Conversation),
            _ => None,
        }
    }
    /// The title a conversation starts with, before its first message.
    pub fn first_title(self) -> &'static str {
        match self {
            Self::Orchestrator => "Orchestrator",
            Self::Conversation => "New conversation",
        }
    }
}

/// A conversation's id: 32 lowercase hexadecimal digits, so it is a
/// directory name and an argument with nothing to escape.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Id(String);

impl Id {
    pub fn parse(text: &str) -> Option<Self> {
        (text.len() == 32 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
            .then(|| Self(text.to_string()))
    }
    /// A fresh id from the kernel's random source.
    pub fn random() -> Result<Self, String> {
        Ok(Self(random_hex(16)?))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// `bytes` random bytes from `/dev/urandom`, in hexadecimal.
pub fn random_hex(bytes: usize) -> Result<String, String> {
    let mut raw = vec![0u8; bytes];
    File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut raw))
        .map_err(|e| format!("/dev/urandom: {e}"))?;
    Ok(raw.iter().map(|b| format!("{b:02x}")).collect())
}

/// Seconds since the epoch, for the records' times.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The state directory and the paths under it.
#[derive(Clone, Debug)]
pub struct StateDir {
    root: PathBuf,
}

impl StateDir {
    /// `$XDG_STATE_HOME/td-agent`, else `$HOME/.local/state/td-agent`;
    /// either base must be absolute.
    pub fn from_env(
        state_home: Option<std::ffi::OsString>,
        home: Option<std::ffi::OsString>,
    ) -> Result<Self, String> {
        // A relative XDG_STATE_HOME is invalid and ignored, as the XDG
        // base directory rules say; a relative HOME is still refused.
        let base = match state_home.map(PathBuf::from).filter(|v| v.is_absolute()) {
            Some(dir) => dir,
            None => PathBuf::from(home.filter(|v| !v.is_empty()).ok_or(
                "neither XDG_STATE_HOME nor HOME is set: the conversation store has no place",
            )?)
            .join(".local/state"),
        };
        if !base.is_absolute() {
            return Err(format!(
                "{} is not an absolute path: the conversation store needs one",
                base.display()
            ));
        }
        Ok(Self::at(base.join("td-agent")))
    }

    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn conversations(&self) -> PathBuf {
        self.root.join("conversations")
    }

    pub fn conversation(&self, id: &Id) -> PathBuf {
        self.conversations().join(id.as_str())
    }

    /// Makes the state directory and its conversations directory, each
    /// private to the caller, when they are missing.
    pub fn ensure(&self) -> Result<(), String> {
        for dir in [&self.root, &self.conversations()] {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        Ok(())
    }

    /// The window process's lock, which refuses a second window process
    /// over the same state directory (DESIGN.md §2).
    pub fn lock_window(&self) -> Result<File, String> {
        let path = self.root.join("window.lock");
        let file = open_lock(&path)?;
        match file.try_lock() {
            Ok(()) => Ok(file),
            Err(TryLockError::WouldBlock) => Err(format!(
                "another td-agent window is running over {} (it holds {})",
                self.root.display(),
                path.display()
            )),
            Err(TryLockError::Error(e)) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// Every conversation the store holds, by its `meta`, with what could
    /// not be read named rather than skipped silently.
    pub fn list(&self) -> (Vec<Meta>, Vec<String>) {
        let mut metas = Vec::new();
        let mut problems = Vec::new();
        let entries = match std::fs::read_dir(self.conversations()) {
            Ok(entries) => entries,
            Err(e) => {
                problems.push(format!("{}: {e}", self.conversations().display()));
                return (metas, problems);
            }
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(Id::parse) else {
                continue;
            };
            match read_meta(&self.conversation(&id)) {
                Ok(meta) if meta.id == id => metas.push(meta),
                Ok(_) => problems.push(format!("{id}: its meta names another id")),
                // A directory made but not yet written is a conversation
                // being created; its process reports it.
                Err(e) => problems.push(format!("{id}: {e}")),
            }
        }
        metas.sort_by(|a, b| (a.created, &a.id).cmp(&(b.created, &b.id)));
        (metas, problems)
    }

    /// The split's preferred share as the window last saved it, in its
    /// own pixels: first and total.
    pub fn load_share(&self) -> Option<(u32, u32)> {
        let text = read_bounded(&self.root.join("layout"), 256).ok()?;
        let text = std::str::from_utf8(&text).ok()?;
        let rest = text.strip_prefix("share ")?.strip_suffix('\n')?;
        let (first, total) = rest.split_once(' ')?;
        Some((first.parse().ok()?, total.parse().ok()?))
    }

    /// Saves the split's share, replacing the file whole.
    pub fn save_share(&self, first: u32, total: u32) -> Result<(), String> {
        replace(
            &self.root,
            "layout",
            format!("share {first} {total}\n").as_bytes(),
        )
    }
}

/// A lock file, created when missing, never through a final link.
fn open_lock(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// At most `limit` bytes of `path`, opened without following a final
/// link; a longer file is refused.
pub(crate) fn read_bounded(path: &Path, limit: u64) -> std::io::Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW)
        .open(path)?;
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("longer than {limit} bytes"),
        ));
    }
    Ok(bytes)
}

/// `read_bounded`, its failure said with the path.
fn read_named(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    read_bounded(path, limit).map_err(|e| format!("{}: {e}", path.display()))
}

/// Replaces `dir/name` whole: a fresh temporary written and synced, then
/// renamed over it and the directory synced, so a reader sees the old
/// bytes or the new, never part of either.
pub(crate) fn replace(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    let temporary = dir.join(format!(".{name}.{}", std::process::id()));
    let _ = std::fs::remove_file(&temporary);
    let write = || -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(O_NOFOLLOW)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, dir.join(name))?;
        File::open(dir)?.sync_all()
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&temporary);
        format!("{}: {e}", dir.join(name).display())
    })
}

/// A conversation's `meta` (DESIGN.md §6). The workspace, model and
/// parent are null until the increments that give them values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Meta {
    pub id: Id,
    pub role: Role,
    pub title: String,
    pub created: u64,
}

impl Meta {
    fn to_json(&self) -> Json {
        Json::Obj(vec![
            ("version".into(), Json::from(1u32)),
            ("id".into(), Json::Str(self.id.to_string())),
            ("role".into(), Json::Str(self.role.word().into())),
            ("title".into(), Json::Str(self.title.clone())),
            ("workspace".into(), Json::Null),
            ("model".into(), Json::Null),
            ("mode".into(), Json::Null),
            ("parent".into(), Json::Null),
            ("created".into(), Json::from(self.created)),
        ])
    }

    fn from_json(value: &Json) -> Result<Self, String> {
        if value.get("version").and_then(Json::as_u64) != Some(1) {
            return Err("meta is not version 1".into());
        }
        let field = |name: &str| value.get(name).and_then(Json::as_str);
        Ok(Self {
            id: field("id").and_then(Id::parse).ok_or("meta has no id")?,
            role: field("role")
                .and_then(Role::parse)
                .ok_or("meta has no role")?,
            title: field("title").map(title).ok_or("meta has no title")?,
            created: value
                .get("created")
                .and_then(Json::as_u64)
                .ok_or("meta has no creation time")?,
        })
    }
}

fn read_meta(dir: &Path) -> Result<Meta, String> {
    let bytes = read_named(&dir.join("meta"), MAX_META)?;
    let value = crate::json::parse_slice(&bytes).map_err(|e| format!("meta: {e}"))?;
    Meta::from_json(&value)
}

/// A title as the list shows it: the first line of `text`, control
/// characters as spaces, trimmed, at most `MAX_TITLE` characters.
pub fn title(text: &str) -> String {
    text.trim_start()
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_TITLE)
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// The effect a `Started` record opens: a turn, started when a user
/// message is logged and finished when its reply is (DESIGN.md §6,
/// Recovery). A model request is the other effect, opened by its own
/// `Request` record, which carries what the request was.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Effect {
    Turn,
}

impl Effect {
    fn word(self) -> &'static str {
        match self {
            Self::Turn => "turn",
        }
    }
    fn parse(word: &str) -> Option<Self> {
        (word == "turn").then_some(Self::Turn)
    }
}

/// What a model request was for (DESIGN.md §5, §13).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Purpose {
    /// The conversation's own exchange: the prefix and the log's messages.
    Turn,
    /// A title from `title_model` after the first exchange.
    Title,
}

impl Purpose {
    pub fn word(self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::Title => "title",
        }
    }
    fn parse(word: &str) -> Option<Self> {
        match word {
            "turn" => Some(Self::Turn),
            "title" => Some(Self::Title),
            _ => None,
        }
    }
}

/// How a request's recorded cost was known.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Basis {
    /// The response's `usage.cost`.
    Reported,
    /// The response's token counts at the cached prices.
    Computed,
    /// Unknown: the request may have run and been billed before failing,
    /// so its whole reservation is counted as spent.
    Reserved,
    /// The request was not run (refused, rate-limited): nothing.
    Nothing,
}

impl Basis {
    pub fn word(self) -> &'static str {
        match self {
            Self::Reported => "reported",
            Self::Computed => "computed",
            Self::Reserved => "reserved",
            Self::Nothing => "none",
        }
    }
    fn parse(word: &str) -> Option<Self> {
        match word {
            "reported" => Some(Self::Reported),
            "computed" => Some(Self::Computed),
            "reserved" => Some(Self::Reserved),
            "none" => Some(Self::Nothing),
            _ => None,
        }
    }
}

/// One log event's content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Kind {
    /// The human's message, with the window's delivery id, which a
    /// receiver logs once (DESIGN.md §6, Recovery).
    User { delivery: String, text: String },
    /// An effect logged and synced before it runs, for the event `of`.
    Started { effect: Effect, of: u64 },
    /// The effect started at `started` finished; `retry` when the human
    /// may ask for it again (a 502, say: shown, never retried by itself).
    Finished {
        started: u64,
        outcome: String,
        retry: bool,
    },
    /// A restart found the effect started at `started` unfinished; it is
    /// never repeated.
    Interrupted { started: u64 },
    /// Something the store reports about itself, a torn line dropped.
    Notice { text: String },
    /// A request prefix (DESIGN.md §6, §13) superseding the `prefix` file
    /// and any earlier one from here on: a JSON array of the messages
    /// every request of the conversation begins with, as exact text.
    Prefix { text: String },
    /// A model request, logged and synced before it is sent: the turn it
    /// belongs to, what it is for, the prefix it begins with (0 for the
    /// file, else the `Prefix` event's sequence number), the exact text of
    /// its body's other members (`head`), its body's length, and the
    /// credits reserved for it. The body is a pure function of these and
    /// the log before it (`request::body`).
    Request {
        turn: u64,
        purpose: Purpose,
        prefix: u64,
        head: String,
        bytes: u64,
        reserved: u64,
    },
    /// The assistant message a request received: its text, its reasoning
    /// as text where the provider gave it, its `reasoning_details` as the
    /// exact bytes of the response that carried them (from a stream, the
    /// array as assembled, serialized once), and why it ended.
    /// `incomplete` marks what a stream that broke off, failed or was
    /// interrupted had brought: kept and shown, never sent back.
    Assistant {
        request: u64,
        content: Option<String>,
        reasoning: Option<String>,
        details: Option<String>,
        finish: String,
        incomplete: bool,
    },
    /// A request's token counts and what it cost, in pico-credits.
    Usage {
        request: u64,
        tokens: Tokens,
        cost: u64,
        basis: Basis,
    },
    /// The title a title request gave the conversation.
    Title { request: u64, text: String },
}

/// One line of the log.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Event {
    pub seq: u64,
    pub time: u64,
    pub kind: Kind,
}

impl Event {
    pub fn to_json(&self) -> Json {
        let mut pairs: Vec<(String, Json)> = vec![
            ("seq".into(), Json::from(self.seq)),
            ("time".into(), Json::from(self.time)),
        ];
        let mut put = |key: &str, value: Json| pairs.push((key.into(), value));
        match &self.kind {
            Kind::User { delivery, text } => {
                put("kind", Json::Str("user".into()));
                put("delivery", Json::Str(delivery.clone()));
                put("text", Json::Str(text.clone()));
            }
            Kind::Started { effect, of } => {
                put("kind", Json::Str("started".into()));
                put("effect", Json::Str(effect.word().into()));
                put("of", Json::from(*of));
            }
            Kind::Finished {
                started,
                outcome,
                retry,
            } => {
                put("kind", Json::Str("finished".into()));
                put("started", Json::from(*started));
                put("outcome", Json::Str(outcome.clone()));
                if *retry {
                    put("retry", Json::Bool(true));
                }
            }
            Kind::Interrupted { started } => {
                put("kind", Json::Str("interrupted".into()));
                put("started", Json::from(*started));
            }
            Kind::Notice { text } => {
                put("kind", Json::Str("notice".into()));
                put("text", Json::Str(text.clone()));
            }
            Kind::Prefix { text } => {
                put("kind", Json::Str("prefix".into()));
                put("text", Json::Str(text.clone()));
            }
            Kind::Request {
                turn,
                purpose,
                prefix,
                head,
                bytes,
                reserved,
            } => {
                put("kind", Json::Str("request".into()));
                put("turn", Json::from(*turn));
                put("purpose", Json::Str(purpose.word().into()));
                put("prefix", Json::from(*prefix));
                put("head", Json::Str(head.clone()));
                put("bytes", Json::from(*bytes));
                put("reserved", Json::from(*reserved));
            }
            Kind::Assistant {
                request,
                content,
                reasoning,
                details,
                finish,
                incomplete,
            } => {
                let text = |v: &Option<String>| v.clone().map_or(Json::Null, Json::Str);
                put("kind", Json::Str("assistant".into()));
                put("request", Json::from(*request));
                put("content", text(content));
                put("reasoning", text(reasoning));
                put("reasoning_details", text(details));
                put("finish", Json::Str(finish.clone()));
                if *incomplete {
                    put("incomplete", Json::Bool(true));
                }
            }
            Kind::Usage {
                request,
                tokens,
                cost,
                basis,
            } => {
                put("kind", Json::Str("usage".into()));
                put("request", Json::from(*request));
                put("prompt_tokens", Json::from(tokens.prompt));
                put("completion_tokens", Json::from(tokens.completion));
                put("cached_tokens", Json::from(tokens.cached));
                put("cache_write_tokens", Json::from(tokens.cache_write));
                put("reasoning_tokens", Json::from(tokens.reasoning));
                put("cost", Json::from(*cost));
                put("basis", Json::Str(basis.word().into()));
            }
            Kind::Title { request, text } => {
                put("kind", Json::Str("title".into()));
                put("request", Json::from(*request));
                put("text", Json::Str(text.clone()));
            }
        }
        Json::Obj(pairs)
    }

    pub fn from_json(value: &Json) -> Result<Self, String> {
        let number = |name: &str| {
            value
                .get(name)
                .and_then(Json::as_u64)
                .ok_or_else(|| format!("no {name}"))
        };
        let string = |name: &str| {
            value
                .get(name)
                .and_then(Json::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("no {name}"))
        };
        let kind = match value.get("kind").and_then(Json::as_str) {
            Some("user") => Kind::User {
                delivery: string("delivery")?,
                text: string("text")?,
            },
            Some("started") => Kind::Started {
                effect: value
                    .get("effect")
                    .and_then(Json::as_str)
                    .and_then(Effect::parse)
                    .ok_or("no effect")?,
                of: number("of")?,
            },
            Some("finished") => Kind::Finished {
                started: number("started")?,
                outcome: string("outcome")?,
                retry: match value.get("retry") {
                    None => false,
                    Some(retry) => retry.as_bool().ok_or("retry is not a boolean")?,
                },
            },
            Some("interrupted") => Kind::Interrupted {
                started: number("started")?,
            },
            Some("notice") => Kind::Notice {
                text: string("text")?,
            },
            Some("prefix") => Kind::Prefix {
                text: string("text")?,
            },
            Some("request") => Kind::Request {
                turn: number("turn")?,
                purpose: value
                    .get("purpose")
                    .and_then(Json::as_str)
                    .and_then(Purpose::parse)
                    .ok_or("no purpose")?,
                prefix: number("prefix")?,
                head: string("head")?,
                bytes: number("bytes")?,
                reserved: number("reserved")?,
            },
            Some("assistant") => {
                let text = |name: &str| -> Result<Option<String>, String> {
                    match value.get(name) {
                        None | Some(Json::Null) => Ok(None),
                        Some(Json::Str(text)) => Ok(Some(text.clone())),
                        Some(_) => Err(format!("{name} is not a string")),
                    }
                };
                Kind::Assistant {
                    request: number("request")?,
                    content: text("content")?,
                    reasoning: text("reasoning")?,
                    details: text("reasoning_details")?,
                    finish: string("finish")?,
                    incomplete: match value.get("incomplete") {
                        None => false,
                        Some(flag) => flag.as_bool().ok_or("incomplete is not a boolean")?,
                    },
                }
            }
            Some("usage") => Kind::Usage {
                request: number("request")?,
                tokens: Tokens {
                    prompt: number("prompt_tokens")?,
                    completion: number("completion_tokens")?,
                    cached: number("cached_tokens")?,
                    cache_write: number("cache_write_tokens")?,
                    reasoning: number("reasoning_tokens")?,
                },
                cost: number("cost")?,
                basis: value
                    .get("basis")
                    .and_then(Json::as_str)
                    .and_then(Basis::parse)
                    .ok_or("no basis")?,
            },
            Some("title") => Kind::Title {
                request: number("request")?,
                text: string("text")?,
            },
            Some(other) => return Err(format!("unknown kind {other:?}")),
            None => return Err("no kind".into()),
        };
        Ok(Self {
            seq: number("seq")?,
            time: number("time")?,
            kind,
        })
    }
}

/// What opening a conversation found, for its process to report.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Load {
    /// The bytes of a torn final line dropped, when there was one.
    pub torn: Option<u64>,
    /// The effects found started and not finished, each now recorded as
    /// interrupted.
    pub interrupted: Vec<u64>,
}

/// A conversation open for writing: its directory locked, its meta and
/// its log loaded. The conversation process holds exactly one.
#[derive(Debug)]
pub struct Conversation {
    dir: PathBuf,
    meta: Meta,
    log: File,
    /// The log's length, held within `MAX_LOG`.
    length: u64,
    events: Vec<Event>,
    /// The `prefix` file's text, which a `Prefix` event supersedes.
    prefix: String,
    /// Kept for as long as the conversation is open: the lock.
    _lock: File,
}

impl Conversation {
    /// Opens conversation `id`, creating it as `create` when given, after
    /// taking its lock, waiting at most `wait` for a process still exiting
    /// to let it go.
    pub fn open(
        state: &StateDir,
        id: &Id,
        create: Option<Role>,
        wait: Duration,
    ) -> Result<(Self, Load), String> {
        let dir = state.conversation(id);
        if create.is_some() {
            DirBuilder::new()
                .mode(0o700)
                .create(&dir)
                .map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let lock = lock_conversation(&dir, wait)?;
        let meta = match create {
            Some(role) => {
                let meta = Meta {
                    id: id.clone(),
                    role,
                    title: role.first_title().to_string(),
                    created: now(),
                };
                // The request prefix (§13) is written once and never
                // rewritten; a later prefix is a log event.
                create_file(&dir.join("prefix"), crate::prompt::prefix(role).as_bytes())?;
                create_file(&dir.join("log"), b"")?;
                replace(&dir, "meta", meta.to_json().to_string().as_bytes())?;
                meta
            }
            None => read_meta(&dir)?,
        };
        if &meta.id != id {
            return Err(format!("{}: its meta names another id", dir.display()));
        }
        let prefix = String::from_utf8(read_named(&dir.join("prefix"), MAX_PREFIX)?)
            .map_err(|_| format!("{}: not UTF-8", dir.join("prefix").display()))?;
        let path = dir.join("log");
        let mut log = OpenOptions::new()
            .read(true)
            .append(true)
            .custom_flags(O_NOFOLLOW)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let length = log
            .metadata()
            .map_err(|e| format!("{}: {e}", path.display()))?
            .len();
        if length > MAX_LOG {
            return Err(format!(
                "{} is {length} bytes, past the {MAX_LOG}-byte bound",
                path.display()
            ));
        }
        let mut bytes = Vec::new();
        log.read_to_end(&mut bytes)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let (events, kept) = parse_log(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut conversation = Self {
            dir,
            meta,
            log,
            length: bytes.len() as u64,
            events,
            prefix,
            _lock: lock,
        };
        let mut load = Load::default();
        if kept < bytes.len() {
            // A line cut short by a crash: dropped, so the next append
            // starts a line of its own, and reported.
            let dropped = (bytes.len() - kept) as u64;
            conversation
                .log
                .set_len(kept as u64)
                .and_then(|()| conversation.log.sync_all())
                .map_err(|e| format!("{}: {e}", path.display()))?;
            conversation.length = kept as u64;
            conversation.append(Kind::Notice {
                text: format!("dropped a torn final line of {dropped} bytes from the log"),
            })?;
            load.torn = Some(dropped);
        }
        // A user message logged without its turn's start, the process
        // having died between the two lines: its turn is started here so
        // that, like every other, it is recorded as interrupted.
        for of in conversation.unturned() {
            conversation.append(Kind::Started {
                effect: Effect::Turn,
                of,
            })?;
        }
        for started in conversation.unfinished() {
            conversation.append(Kind::Interrupted { started })?;
            load.interrupted.push(started);
        }
        if load.torn.is_some() || !load.interrupted.is_empty() {
            conversation.sync()?;
        }
        Ok((conversation, load))
    }

    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    /// Whether the log has room for a message of `text` bytes and the
    /// `LOG_RESERVE` after it, at its longest once escaped, so that an
    /// accepted message never leaves the log past what an open reads.
    pub fn has_room(&self, text: usize) -> bool {
        let line = (text as u64).saturating_mul(6).saturating_add(1024);
        self.length.saturating_add(line).saturating_add(LOG_RESERVE) <= MAX_LOG
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// The `prefix` file's text, as written at creation.
    pub fn prefix_file(&self) -> &str {
        &self.prefix
    }

    /// Whether the log has room for `bytes` more of records and the
    /// `LOG_RESERVE` after them: a reply is taken only while it would fit.
    pub fn has_room_for(&self, bytes: u64) -> bool {
        self.length
            .saturating_add(bytes)
            .saturating_add(LOG_RESERVE)
            <= MAX_LOG
    }

    /// Whether a user message with this delivery id is already logged.
    pub fn delivered(&self, delivery: &str) -> bool {
        self.events
            .iter()
            .any(|e| matches!(&e.kind, Kind::User { delivery: d, .. } if d == delivery))
    }

    /// Every user message no turn was started for.
    fn unturned(&self) -> Vec<u64> {
        let mut users = Vec::new();
        for event in &self.events {
            match &event.kind {
                Kind::User { .. } => users.push(event.seq),
                Kind::Started { of, .. } => users.retain(|seq| seq != of),
                _ => {}
            }
        }
        users
    }

    /// Every effect started and neither finished nor interrupted.
    fn unfinished(&self) -> Vec<u64> {
        let mut open = Vec::new();
        for event in &self.events {
            match &event.kind {
                Kind::Started { .. } | Kind::Request { .. } => open.push(event.seq),
                Kind::Finished { started, .. } | Kind::Interrupted { started } => {
                    open.retain(|seq| seq != started)
                }
                _ => {}
            }
        }
        open
    }

    /// Appends one event, written whole in a single write; not synced
    /// until `sync`.
    pub fn append(&mut self, kind: Kind) -> Result<&Event, String> {
        let seq = self
            .events
            .last()
            .map_or(Some(1), |last| last.seq.checked_add(1))
            .ok_or("the log's sequence is exhausted")?;
        let event = Event {
            seq,
            time: now(),
            kind,
        };
        let mut line = event.to_json().to_string().into_bytes();
        if line.len() > MAX_LINE {
            return Err(format!(
                "an event of {} bytes is past the bound",
                line.len()
            ));
        }
        line.push(b'\n');
        let length = self
            .length
            .checked_add(line.len() as u64)
            .filter(|length| *length <= MAX_LOG)
            .ok_or_else(|| format!("the log would pass its {MAX_LOG}-byte bound"))?;
        self.log
            .write_all(&line)
            .map_err(|e| format!("{}: {e}", self.dir.join("log").display()))?;
        self.length = length;
        self.events.push(event);
        self.events
            .last()
            .ok_or_else(|| "the log lost an event".into())
    }

    /// Makes every append so far durable: a turn boundary, or a started
    /// record before its effect runs.
    pub fn sync(&mut self) -> Result<(), String> {
        self.log
            .sync_data()
            .map_err(|e| format!("{}: {e}", self.dir.join("log").display()))
    }

    /// Retitles the conversation, replacing `meta` whole.
    pub fn retitle(&mut self, text: &str) -> Result<(), String> {
        let mut meta = self.meta.clone();
        meta.title = title(text);
        if meta.title.is_empty() {
            return Ok(());
        }
        replace(&self.dir, "meta", meta.to_json().to_string().as_bytes())?;
        self.meta = meta;
        Ok(())
    }
}

/// Takes the conversation directory's lock, retrying while it is held
/// until `wait` has passed.
fn lock_conversation(dir: &Path, wait: Duration) -> Result<File, String> {
    let path = dir.join("lock");
    let file = open_lock(&path)?;
    let deadline = Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(TryLockError::WouldBlock) => {
                return Err(format!(
                    "another process holds {}: the conversation already has its writer",
                    path.display()
                ))
            }
            Err(TryLockError::Error(e)) => return Err(format!("{}: {e}", path.display())),
        }
    }
}

/// A new file holding `bytes`, which must not exist yet.
fn create_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// The events of a log and the length of its whole lines: what follows
/// the last newline is a torn line for the caller to drop. A whole line
/// that does not parse, or a sequence that skips, is corruption and
/// refused, not dropped.
pub fn parse_log(bytes: &[u8]) -> Result<(Vec<Event>, usize), String> {
    let mut events: Vec<Event> = Vec::new();
    let mut at = 0usize;
    for (number, line) in bytes.split_inclusive(|b| *b == b'\n').enumerate() {
        let Some(body) = line.strip_suffix(b"\n") else {
            break;
        };
        if body.len() > MAX_LINE {
            return Err(format!("line {} is past the bound", number + 1));
        }
        let value = crate::json::parse_slice(body)
            .map_err(|e| format!("line {} is not an event: {e}", number + 1))?;
        let event = Event::from_json(&value).map_err(|e| format!("line {}: {e}", number + 1))?;
        let expected = events.last().map_or(1, |last| last.seq.saturating_add(1));
        if event.seq != expected {
            return Err(format!(
                "line {} has sequence number {} where {expected} belongs",
                number + 1,
                event.seq
            ));
        }
        events.push(event);
        at = at.saturating_add(line.len());
    }
    Ok((events, at))
}

#[cfg(test)]
pub mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    /// A private directory removed when dropped.
    pub struct Scratch(pub PathBuf);
    impl Scratch {
        pub fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "td-agent-{tag}-{}-{}",
                std::process::id(),
                random_hex(4).unwrap()
            ));
            DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
        pub fn state(&self) -> StateDir {
            let state = StateDir::at(self.0.join("td-agent"));
            state.ensure().unwrap();
            state
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn user(text: &str) -> Kind {
        Kind::User {
            delivery: random_hex(8).unwrap(),
            text: text.into(),
        }
    }

    #[test]
    fn the_state_directory_follows_xdg_and_refuses_a_relative_base() {
        let state = StateDir::from_env(Some("/s".into()), Some("/h".into())).unwrap();
        assert_eq!(state.root(), Path::new("/s/td-agent"));
        let state = StateDir::from_env(None, Some("/h".into())).unwrap();
        assert_eq!(state.root(), Path::new("/h/.local/state/td-agent"));
        let state = StateDir::from_env(Some("".into()), Some("/h".into())).unwrap();
        assert_eq!(state.root(), Path::new("/h/.local/state/td-agent"));
        assert!(StateDir::from_env(Some("rel".into()), None).is_err());
        let state = StateDir::from_env(Some("rel".into()), Some("/h".into())).unwrap();
        assert_eq!(state.root(), Path::new("/h/.local/state/td-agent"));
        assert!(StateDir::from_env(None, None).is_err());
    }

    #[test]
    fn ids_are_32_lowercase_hex_digits() {
        let id = Id::random().unwrap();
        assert!(Id::parse(id.as_str()).is_some());
        assert_ne!(id, Id::random().unwrap());
        for bad in [
            "",
            "../x",
            &"A".repeat(32),
            &"a".repeat(31),
            &"a".repeat(33),
        ] {
            assert!(Id::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn a_created_conversation_replays_exactly_after_reopening() {
        let scratch = Scratch::new("replay");
        let state = scratch.state();
        let id = Id::random().unwrap();
        let written = {
            let (mut conversation, load) =
                Conversation::open(&state, &id, Some(Role::Conversation), LOCK_WAIT).unwrap();
            assert_eq!(load, Load::default());
            assert_eq!(conversation.meta().title, "New conversation");
            conversation
                .append(user("hello\nthere \"quoted\""))
                .unwrap();
            conversation
                .append(Kind::Started {
                    effect: Effect::Turn,
                    of: 1,
                })
                .unwrap();
            conversation
                .append(Kind::Finished {
                    started: 2,
                    outcome: "no model".into(),
                    retry: false,
                })
                .unwrap();
            conversation.sync().unwrap();
            conversation.retitle("hello\nthere").unwrap();
            conversation.events().to_vec()
        };
        let dir = state.conversation(&id);
        assert_eq!(
            std::fs::read_to_string(dir.join("prefix")).unwrap(),
            crate::prompt::prefix(Role::Conversation)
        );

        let log = std::fs::read_to_string(dir.join("log")).unwrap();
        assert_eq!(log.lines().count(), 3);
        assert!(log.lines().all(|line| line.starts_with("{\"seq\":")));
        let (conversation, load) = Conversation::open(&state, &id, None, LOCK_WAIT).unwrap();
        assert_eq!(load, Load::default());
        assert_eq!(conversation.events(), written.as_slice());
        assert_eq!(conversation.meta().title, "hello");
        let (metas, problems) = state.list();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(metas, [conversation.meta().clone()]);
    }

    #[test]
    fn a_log_near_its_bound_refuses_what_would_pass_it() {
        let scratch = Scratch::new("bound");
        let state = scratch.state();
        let id = Id::random().unwrap();
        let (mut conversation, _) =
            Conversation::open(&state, &id, Some(Role::Conversation), LOCK_WAIT).unwrap();
        assert!(conversation.has_room(128 * 1024));
        // As if the log had grown to its reserve.
        conversation.length = MAX_LOG - LOG_RESERVE;
        assert!(!conversation.has_room(1));
        // The reserve still takes the records that follow a message.
        conversation.append(user("in the reserve")).unwrap();
        conversation.length = MAX_LOG - 10;
        assert!(conversation.append(user("past it")).is_err());
        assert_eq!(conversation.events().len(), 1);
    }

    #[test]
    fn a_torn_final_line_is_dropped_reported_and_appended_after() {
        let scratch = Scratch::new("torn");
        let state = scratch.state();
        let id = Id::random().unwrap();
        {
            let (mut conversation, _) =
                Conversation::open(&state, &id, Some(Role::Conversation), LOCK_WAIT).unwrap();
            conversation.append(user("kept")).unwrap();
            conversation.sync().unwrap();
        }
        let path = state.conversation(&id).join("log");
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        let torn = b"{\"seq\":2,\"time\":1,\"kind\":\"us";
        file.write_all(torn).unwrap();
        drop(file);
        let (conversation, load) = Conversation::open(&state, &id, None, LOCK_WAIT).unwrap();
        assert_eq!(load.torn, Some(torn.len() as u64));
        // The user message was logged without its turn's start, as when
        // the process dies between the two lines: its turn is started and
        // recorded as interrupted, like any other turn a restart cut.
        let kinds: Vec<&Kind> = conversation.events().iter().map(|e| &e.kind).collect();
        assert!(
            matches!(kinds.as_slice(), [
                Kind::User { .. },
                Kind::Notice { text },
                Kind::Started { of: 1, .. },
                Kind::Interrupted { started: 3 },
            ] if text.contains("28 bytes")),
            "{kinds:?}"
        );
        assert_eq!(load.interrupted, [3]);
        drop(conversation);
        // The notice is a line of its own: the log loads whole again.
        let (conversation, load) = Conversation::open(&state, &id, None, LOCK_WAIT).unwrap();
        assert_eq!(load, Load::default());
        assert_eq!(conversation.events().len(), 4);
    }

    #[test]
    fn corruption_inside_the_log_is_refused_not_dropped() {
        let good = "{\"seq\":1,\"time\":1,\"kind\":\"notice\",\"text\":\"a\"}\n";
        assert_eq!(parse_log(good.as_bytes()).unwrap().1, good.len());
        for bad in [
            "not json\n".to_string(),
            format!("{good}{good}"),
            "{\"seq\":2,\"time\":1,\"kind\":\"notice\",\"text\":\"a\"}\n".into(),
            "{\"seq\":1,\"time\":1,\"kind\":\"sing\"}\n".into(),
            "{\"seq\":1,\"time\":1,\"kind\":\"user\",\"text\":\"no delivery\"}\n".into(),
        ] {
            assert!(parse_log(bad.as_bytes()).is_err(), "{bad}");
        }
        // Only what follows the last newline is torn.
        let (events, kept) = parse_log(format!("{good}garbage").as_bytes()).unwrap();
        assert_eq!((events.len(), kept), (1, good.len()));
    }

    #[test]
    fn an_effect_started_and_not_finished_is_interrupted_once_never_repeated() {
        let scratch = Scratch::new("interrupted");
        let state = scratch.state();
        let id = Id::random().unwrap();
        {
            let (mut conversation, _) =
                Conversation::open(&state, &id, Some(Role::Conversation), LOCK_WAIT).unwrap();
            conversation.append(user("one")).unwrap();
            conversation
                .append(Kind::Started {
                    effect: Effect::Turn,
                    of: 1,
                })
                .unwrap();
            conversation.sync().unwrap();
            // The process dies here, mid-turn.
        }
        let (conversation, load) = Conversation::open(&state, &id, None, LOCK_WAIT).unwrap();
        assert_eq!(load.interrupted, [2]);
        assert!(matches!(
            conversation.events().last().map(|e| &e.kind),
            Some(Kind::Interrupted { started: 2 })
        ));
        drop(conversation);
        let (conversation, load) = Conversation::open(&state, &id, None, LOCK_WAIT).unwrap();
        assert!(load.interrupted.is_empty(), "recorded once");
        assert_eq!(conversation.events().len(), 3);
    }

    #[test]
    fn a_held_conversation_refuses_a_second_writer() {
        let scratch = Scratch::new("lock");
        let state = scratch.state();
        let id = Id::random().unwrap();
        let (first, _) =
            Conversation::open(&state, &id, Some(Role::Conversation), LOCK_WAIT).unwrap();
        let second = Conversation::open(&state, &id, None, Duration::from_millis(50)).unwrap_err();
        assert!(second.contains("already has its writer"), "{second}");
        // Creating it again is refused before the lock: it exists.
        assert!(Conversation::open(&state, &id, Some(Role::Conversation), Duration::ZERO).is_err());
        drop(first);
        assert!(Conversation::open(&state, &id, None, Duration::ZERO).is_ok());
    }

    #[test]
    fn a_second_window_lock_is_refused_while_the_first_is_held() {
        let scratch = Scratch::new("window");
        let state = scratch.state();
        let held = state.lock_window().unwrap();
        let refused = state.lock_window().unwrap_err();
        assert!(refused.contains("another td-agent window"), "{refused}");
        drop(held);
        assert!(state.lock_window().is_ok());
    }

    #[test]
    fn delivery_ids_are_logged_once() {
        let scratch = Scratch::new("delivery");
        let state = scratch.state();
        let id = Id::random().unwrap();
        let (mut conversation, _) =
            Conversation::open(&state, &id, Some(Role::Orchestrator), LOCK_WAIT).unwrap();
        assert!(!conversation.delivered("d1"));
        conversation
            .append(Kind::User {
                delivery: "d1".into(),
                text: "x".into(),
            })
            .unwrap();
        assert!(conversation.delivered("d1"));
        assert!(!conversation.delivered("d2"));
    }

    #[test]
    fn the_share_is_saved_whole_and_read_back() {
        let scratch = Scratch::new("share");
        let state = scratch.state();
        assert_eq!(state.load_share(), None);
        state.save_share(300, 1000).unwrap();
        assert_eq!(state.load_share(), Some((300, 1000)));
        std::fs::write(state.root().join("layout"), "share x 1\n").unwrap();
        assert_eq!(state.load_share(), None);
    }

    #[test]
    fn titles_are_one_bounded_line() {
        assert_eq!(title("  first line\nsecond"), "first line");
        assert_eq!(title("tab\there"), "tab here");
        assert_eq!(title(&"x".repeat(200)).chars().count(), MAX_TITLE);
        assert_eq!(title("\n\n"), "");
    }

    #[test]
    fn a_symlinked_log_is_not_followed() {
        let scratch = Scratch::new("symlink");
        let state = scratch.state();
        let id = Id::random().unwrap();
        drop(Conversation::open(&state, &id, Some(Role::Conversation), LOCK_WAIT).unwrap());
        let dir = state.conversation(&id);
        let elsewhere = scratch.0.join("elsewhere");
        std::fs::write(&elsewhere, b"").unwrap();
        std::fs::remove_file(dir.join("log")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.join("log")).unwrap();
        assert!(Conversation::open(&state, &id, None, LOCK_WAIT).is_err());
    }
}
