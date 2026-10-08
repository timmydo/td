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
//! no stale lock to judge, and std opens every file close-on-exec, so no
//! program a child execs keeps one; a child forked while one is held
//! holds it too until that exec. Files are opened without following a
//! final symbolic link.

use std::fs::{DirBuilder, File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::cost::Tokens;
use crate::workspace::Workspace;
use td_json::Json;

/// `O_NOFOLLOW` on x86-64, as td-ui's control socket spells it.
#[cfg(not(target_arch = "aarch64"))]
pub(crate) const O_NOFOLLOW: i32 = 0o400000;
#[cfg(target_arch = "aarch64")]
pub(crate) const O_NOFOLLOW: i32 = 0o100000;

/// The longest `meta` read back.
const MAX_META: u64 = 64 * 1024;
/// The longest `prefix` read back.
pub const MAX_PREFIX: u64 = 1024 * 1024;
/// The file holding the window's default model.
const DEFAULT_MODEL: &str = "default-model";
/// The name a conversation's directory takes while it is deleted.
const DELETING: &str = ".deleting-";
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

/// What a conversation was created as. Every conversation td-agent makes
/// now is a `Conversation`; an `Orchestrator`, which td-agent once made at
/// startup, is read from an older state directory and is an ordinary
/// conversation like any other (DESIGN.md §3).
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

/// The time now in milliseconds since the epoch, by which conversations
/// are ordered by activity.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// When conversation `meta` was last active, in milliseconds since the
/// epoch: when its log last changed, else when it was made.
pub fn activity(state: &StateDir, meta: &Meta) -> u64 {
    std::fs::metadata(state.conversation(&meta.id).join("log"))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(meta.created.saturating_mul(1000), |d| {
            u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
        })
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
        let state = td_ui::xdg::Base::State;
        let base = td_ui::xdg::dir(state, state_home.as_deref(), home.as_deref())
            .ok_or_else(|| format!("{}: the conversation store has no place", state.missing()))?;
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

    /// Where conversation `id`'s `git_push` writes its export's pack and
    /// the git worker imports it from (DESIGN.md §9, Pushing): in the
    /// conversation's own directory, which no jail can write.
    pub fn push_pack(&self, id: &Id) -> PathBuf {
        self.conversation(id).join("push.pack")
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

    /// Deletes conversation `id` for good (DESIGN.md §6). Its process
    /// must have ended: the conversation's lock is taken first, waiting a
    /// moment for a process just killed to exit, and held to the end. The
    /// directory is renamed out of the list, then removed. An error means
    /// the conversation is untouched; once renamed it is deleted, and a
    /// removal that failed, which the `Some` says, the window's
    /// `sweep_deleted` finishes at its next start.
    pub fn delete(&self, id: &Id) -> Result<Option<String>, String> {
        let dir = self.conversation(id);
        // One whose directory was never made, or is gone, is deleted.
        match std::fs::symlink_metadata(&dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", dir.display())),
            Ok(_) => {}
        }
        let lock = lock_conversation(&dir, Duration::from_secs(2))?;
        let gone = self.conversations().join(format!("{DELETING}{id}"));
        std::fs::rename(&dir, &gone).map_err(|e| format!("{}: {e}", dir.display()))?;
        let removed = File::open(self.conversations())
            .and_then(|d| d.sync_all())
            .and_then(|()| match std::fs::remove_dir_all(&gone) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                removed => removed,
            })
            .map_err(|e| format!("{}: {e}", gone.display()));
        // Its jail directory, its scratch workspace with it, out of the
        // way at once and removed on a thread of its own, since a large
        // tree takes time; what is left the next start sweeps. A
        // directory workspace is the human's and lies elsewhere.
        let jail = crate::workspace::jail_dir(self, id);
        let moved = match self.doom(&jail, id) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("{}: {e}", jail.display())),
            Ok(doomed) => {
                let removing = doomed.clone();
                let spawned = std::thread::Builder::new()
                    .name("td-agent-jail-removal".into())
                    .spawn(move || {
                        if let Err(e) = crate::workspace::remove_tree(&removing) {
                            eprintln!("td-agent: {}: {e}", removing.display());
                        }
                    });
                spawned
                    .map(drop)
                    .map_err(|e| format!("{}: {e}", doomed.display()))
            }
        };
        drop(lock);
        let problems: Vec<String> = removed.err().into_iter().chain(moved.err()).collect();
        Ok((!problems.is_empty()).then(|| problems.join("; ")))
    }

    /// Renames conversation `id`'s jail directory `jail` out of the way,
    /// to be removed: `.deleting-<id>`, or, where an earlier removal left
    /// that name, `.deleting-<id>.<random>`.
    fn doom(&self, jail: &Path, id: &Id) -> std::io::Result<PathBuf> {
        let jails = self.root().join(crate::workspace::JAIL);
        let plain = jails.join(format!("{DELETING}{id}"));
        // EEXIST and ENOTEMPTY: the name is taken.
        match std::fs::rename(jail, &plain) {
            Err(e) if matches!(e.raw_os_error(), Some(17 | 39)) => {
                let suffix = random_hex(4).map_err(std::io::Error::other)?;
                let to = jails.join(format!("{DELETING}{id}.{suffix}"));
                std::fs::rename(jail, &to).map(|()| to)
            }
            moved => moved.map(|()| plain),
        }
    }

    /// Finishes the deletions a crash or a failed removal left, by the
    /// window alone, at its start (DESIGN.md §6); one whose lock is held
    /// is under way and left. What could not be removed is named.
    pub fn sweep_deleted(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let Ok(entries) = std::fs::read_dir(self.conversations()) else {
            return problems;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(gone) = name.to_str().filter(|n| n.starts_with(DELETING)) else {
                continue;
            };
            let path = entry.path();
            if let Ok(lock) = lock_conversation(&path, Duration::ZERO) {
                match std::fs::remove_dir_all(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => problems.push(format!("{gone}: {e}")),
                }
                drop(lock);
            }
        }
        // A jail directory whose conversation is gone, one whose removal
        // failed, and one being removed when the window last closed: the
        // first renamed out of the way, and all removed on a thread of
        // their own, since a large tree takes time. None is being made
        // while the window, which alone sweeps, starts.
        let jails = self.root().join(crate::workspace::JAIL);
        let mut doomed = Vec::new();
        for entry in std::fs::read_dir(&jails).into_iter().flatten().flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if let Some(own) = name.strip_prefix(DELETING) {
                if own.split('.').next().and_then(Id::parse).is_some() {
                    doomed.push(entry.path());
                }
                continue;
            }
            let Some(id) = Id::parse(name) else {
                continue;
            };
            match std::fs::symlink_metadata(self.conversation(&id)) {
                // Only a conversation surely gone: any other failure to
                // look leaves its work alone, and says so.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    match self.doom(&entry.path(), &id) {
                        Ok(to) => doomed.push(to),
                        Err(e) => problems.push(format!("{}: {e}", entry.path().display())),
                    }
                }
                Err(e) => problems.push(format!("{id}: {e}")),
                Ok(_) => {}
            }
        }
        if !doomed.is_empty() {
            let spawned = std::thread::Builder::new()
                .name("td-agent-jail-sweep".into())
                .spawn(move || {
                    for path in doomed {
                        if let Err(e) = crate::workspace::remove_tree(&path) {
                            eprintln!("td-agent: {}: {e}", path.display());
                        }
                    }
                });
            if let Err(e) = spawned {
                problems.push(format!("the jail directories' removal: {e}"));
            }
        }
        problems
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

    /// Marks conversation `id` archived, or not, in its `meta` (DESIGN.md
    /// §7), under its lock, waiting `wait` for it: the window's to write
    /// once the conversation's process has ended, which held it.
    pub fn set_archived(
        &self,
        id: &Id,
        archived: bool,
        removed: bool,
        wait: Duration,
    ) -> Result<(), String> {
        let dir = self.conversation(id);
        let lock = lock_conversation(&dir, wait)?;
        let written = read_meta(&dir).and_then(|mut meta| {
            meta.archived = archived;
            meta.removed |= removed;
            replace(&dir, "meta", meta.to_json().to_string().as_bytes())
        });
        // Its background processes' output is kept until it is archived
        // (DESIGN.md §12), and only then; `remove_dir_all` follows no link.
        let cleared = if archived && written.is_ok() {
            let outputs = dir.join(crate::output::DIR);
            match std::fs::remove_dir_all(&outputs) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(format!(
                    "archived, but its background output {} was not removed: {e}",
                    outputs.display()
                )),
            }
        } else {
            Ok(())
        };
        drop(lock);
        written.and(cleared)
    }

    /// The workspace conversation `id`'s `meta` records, as stored.
    pub fn workspace(&self, id: &Id) -> Result<Option<crate::workspace::Workspace>, String> {
        read_meta(&self.conversation(id)).map(|meta| meta.workspace)
    }

    /// Conversation `id`'s recorded project instructions, as its process
    /// wrote them (DESIGN.md §13).
    pub fn instructions(&self, id: &Id) -> Result<Vec<Instructed>, String> {
        read_instructions(&self.conversation(id))
    }

    /// The workspace repositories conversation `id`'s `meta` says are
    /// prepared.
    pub fn prepared(&self, id: &Id) -> Result<Vec<PathBuf>, String> {
        read_meta(&self.conversation(id)).map(|meta| meta.prepared)
    }

    /// Whether conversation `id`'s `meta` says its workspace went with its
    /// archive.
    pub fn removed(&self, id: &Id) -> Result<bool, String> {
        read_meta(&self.conversation(id)).map(|meta| meta.removed)
    }

    /// Whether conversation `id`'s `meta` says it is archived, as stored.
    pub fn archived(&self, id: &Id) -> Result<bool, String> {
        read_meta(&self.conversation(id)).map(|meta| meta.archived)
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

    /// The default model the window last set (DESIGN.md §4) and the
    /// configuration's `model` key it was set over, none when the key was
    /// left out: `model <id>` then `over <id>` or a bare `over`. Anything
    /// else is no default.
    pub fn load_default_model(&self) -> Option<(String, Option<String>)> {
        // Two model ids and their keys, at the longest.
        let bound = 2 * crate::config::MAX_NAME as u64 + 16;
        let text = read_bounded(&self.root.join(DEFAULT_MODEL), bound).ok()?;
        let text = std::str::from_utf8(&text).ok()?;
        let (model, over) = text.strip_suffix('\n')?.split_once('\n')?;
        let model = crate::config::model_id("model", model.strip_prefix("model ")?).ok()?;
        let over = match over.strip_prefix("over")? {
            "" => None,
            id => Some(crate::config::model_id("model", id.strip_prefix(' ')?).ok()?),
        };
        Some((model, over))
    }

    /// Saves the default model, set over the configuration's `model` key
    /// `over`, replacing the file whole.
    pub fn save_default_model(&self, model: &str, over: Option<&str>) -> Result<(), String> {
        crate::config::model_id("the default model", model)?;
        let over = match over {
            Some(id) => format!("over {}", crate::config::model_id("`model`", id)?),
            None => "over".to_string(),
        };
        replace(
            &self.root,
            DEFAULT_MODEL,
            format!("model {model}\n{over}\n").as_bytes(),
        )
    }

    /// Forgets the default model: the configuration's `model` key changed
    /// since it was chosen, and is the newer.
    pub fn forget_default_model(&self) -> Result<(), String> {
        let path = self.root.join(DEFAULT_MODEL);
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(format!("{}: {e}", path.display()))
            }
            _ => Ok(()),
        }
    }

    /// The remotes the human admitted on a card (DESIGN.md §7), one URL a
    /// line as td-agent records a remote, oldest first; none when there
    /// is no file. A line that is not such a URL refuses the file.
    pub fn load_admitted(&self) -> Result<Vec<String>, String> {
        let path = self.root.join(ADMITTED);
        let bytes = match read_bounded(&path, MAX_ADMITTED_BYTES) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            read => read.map_err(|e| format!("{}: {e}", path.display()))?,
        };
        let text =
            String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8", path.display()))?;
        if text.lines().count() > MAX_ADMITTED {
            return Err(format!(
                "{} holds more than {MAX_ADMITTED} remotes",
                path.display()
            ));
        }
        text.lines()
            .map(|line| match crate::git::Remote::parse(line) {
                Ok(remote) if remote.url() == line => Ok(line.to_string()),
                _ => Err(format!(
                    "{}: {line:?} is not a remote as td-agent records one",
                    path.display()
                )),
            })
            .collect()
    }

    /// The human's rules file (DESIGN.md §11), whole, empty when there is
    /// none; a file `rules::parse_human` refuses is refused, and why.
    pub fn load_rules(&self) -> Result<String, String> {
        let path = self.root.join(crate::rules::HUMAN_FILE);
        let bytes = match read_bounded(&path, crate::rules::MAX_HUMAN_FILE as u64) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
            read => read.map_err(|e| format!("{}: {e}", path.display()))?,
        };
        let text =
            String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8", path.display()))?;
        crate::rules::parse_human(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(text)
    }

    /// Replaces the human's rules file with `text`, which
    /// `rules::parse_human` must take.
    pub fn save_rules(&self, text: &str) -> Result<(), String> {
        crate::rules::parse_human(text)?;
        replace(&self.root, crate::rules::HUMAN_FILE, text.as_bytes())
    }

    /// Moves the remotes file aside, to `remotes.set-aside-<time>`, after
    /// it could not be read: no remote in it is admitted, and a card can
    /// admit again. Where it went.
    pub fn set_admitted_aside(&self) -> Result<PathBuf, String> {
        let from = self.root.join(ADMITTED);
        let to = self.root.join(format!("{ADMITTED}.set-aside-{}", now()));
        std::fs::rename(&from, &to).map_err(|e| format!("{}: {e}", from.display()))?;
        Ok(to)
    }

    /// Adds `remotes` to those admitted on a card, each once, replacing
    /// the file whole; what is admitted then.
    pub fn admit(&self, remotes: &[String]) -> Result<Vec<String>, String> {
        let mut admitted = self.load_admitted()?;
        for remote in remotes {
            let url = crate::git::Remote::parse(remote)?.url();
            if !admitted.contains(&url) {
                admitted.push(url);
            }
        }
        if admitted.len() > MAX_ADMITTED {
            return Err(format!(
                "more than {MAX_ADMITTED} remotes admitted on cards: list a prefix in `remotes` in the configuration"
            ));
        }
        let text: String = admitted.iter().map(|url| format!("{url}\n")).collect();
        replace(&self.root, ADMITTED, text.as_bytes())?;
        Ok(admitted)
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

/// The templates made in the window (DESIGN.md §7): the file, and the
/// most bytes it may take.
const TEMPLATES: &str = "templates";
const MAX_TEMPLATES_BYTES: u64 = 1024 * 1024;

impl StateDir {
    /// The templates made in the window, as `config::templates_from_json`
    /// reads them; none when there is no file. A file it refuses is
    /// refused whole, and why.
    pub fn load_templates(&self) -> Result<Vec<crate::config::Template>, String> {
        let path = self.root.join(TEMPLATES);
        let bytes = match read_bounded(&path, MAX_TEMPLATES_BYTES) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            read => read.map_err(|e| format!("{}: {e}", path.display()))?,
        };
        let text =
            String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8", path.display()))?;
        let value = td_json::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        crate::config::templates_from_json(&value).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Replaces the templates made in the window with `templates`, which
    /// must read back as written.
    pub fn save_templates(&self, templates: &[crate::config::Template]) -> Result<(), String> {
        let value = crate::config::templates_json(templates);
        if crate::config::templates_from_json(&value)? != templates {
            return Err("a template that would not read back as written".into());
        }
        let text = format!("{value}\n");
        if text.len() as u64 > MAX_TEMPLATES_BYTES {
            return Err(format!(
                "the templates are past {MAX_TEMPLATES_BYTES} bytes"
            ));
        }
        replace(&self.root, TEMPLATES, text.as_bytes())
    }

    /// Moves the templates file aside, to `templates.set-aside-<time>`,
    /// after it could not be read, so saving one does not lose the rest.
    pub fn set_templates_aside(&self) -> Result<PathBuf, String> {
        let from = self.root.join(TEMPLATES);
        let to = self.root.join(format!("{TEMPLATES}.set-aside-{}", now()));
        std::fs::rename(&from, &to).map_err(|e| format!("{}: {e}", from.display()))?;
        Ok(to)
    }
}

/// The remotes admitted on cards: the file, and how many it holds, each
/// at most a remote's longest text and its newline.
const ADMITTED: &str = "remotes";
const MAX_ADMITTED: usize = 256;
const MAX_ADMITTED_BYTES: u64 = (MAX_ADMITTED * (crate::git::MAX_TEXT + 1)) as u64;

/// One worktree's project instructions, as a conversation records them:
/// the checkout they are for, the commit they were read at, and what was
/// read there (DESIGN.md §13).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Instructed {
    pub checkout: PathBuf,
    pub base: String,
    pub read: crate::repo::Instructions,
    /// Its repository's `.td-agent/rules` there (§11).
    pub rules: crate::rules::Read,
}

impl Instructed {
    /// Its JSON, its read and its rules each given whole or as the index
    /// of an earlier entry with the same (`instructions_json`).
    fn to_json(&self, same: Option<usize>, same_rules: Option<usize>) -> Json {
        let read = match same {
            Some(index) => ("same".into(), Json::from(index as u64)),
            None => ("read".into(), self.read.to_json()),
        };
        let rules = match same_rules {
            Some(index) => ("same_rules".into(), Json::from(index as u64)),
            None => ("rules".into(), self.rules.to_json()),
        };
        Json::Obj(vec![
            (
                "checkout".into(),
                Json::Str(self.checkout.to_string_lossy().into_owned()),
            ),
            ("base".into(), Json::Str(self.base.clone())),
            read,
            rules,
        ])
    }

    /// The entry at `at` of a file whose entries before it are
    /// `earlier`, which one naming its read takes it from.
    fn from_json(value: &Json, earlier: &[Instructed], at: usize) -> Result<Self, String> {
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Json::as_str)
                .ok_or_else(|| format!("recorded instructions without `{key}`"))
        };
        let checkout = PathBuf::from(text("checkout")?);
        if !checkout.is_absolute() {
            return Err("recorded instructions for a relative checkout".into());
        }
        let base = text("base")?;
        if !crate::git::object_id(base) {
            return Err("recorded instructions at no commit".into());
        }
        let read = match value.get("same") {
            Some(same) => same
                .as_u64()
                .and_then(|index| usize::try_from(index).ok())
                .filter(|index| *index < at)
                .and_then(|index| earlier.get(index))
                .filter(|other| other.base == base)
                .map(|other| other.read.clone())
                .ok_or("recorded instructions naming no earlier entry at their commit")?,
            None => crate::repo::Instructions::from_json(
                value
                    .get("read")
                    .ok_or("recorded instructions without `read`")?,
            )?,
        };
        // A record from before rules were read says nothing of them, so
        // they are not read, and every acting call asks.
        let rules = match (value.get("same_rules"), value.get("rules")) {
            (Some(same), _) => same
                .as_u64()
                .and_then(|index| usize::try_from(index).ok())
                .filter(|index| *index < at)
                .and_then(|index| earlier.get(index))
                .filter(|other| other.base == base)
                .map(|other| other.rules.clone())
                .ok_or("recorded rules naming no earlier entry at their commit")?,
            (None, Some(rules)) => crate::rules::Read::from_json(rules)?,
            (None, None) => {
                crate::rules::Read::unread("recorded before td-agent read repository rules")
            }
        };
        Ok(Self {
            checkout,
            base: base.to_string(),
            read,
            rules,
        })
    }
}

/// The conversation's recorded project instructions, its file and the
/// most text they hold in all, and the file's bound, that text escaped.
/// The prefix holds them, its JSON escaped again in its log event, so a
/// byte of them takes at most four there (`prompt::project` makes
/// controls plain): 128 KiB leaves the line room for the rest.
const INSTRUCTIONS: &str = "instructions";
pub const MAX_INSTRUCTED: usize = 128 * 1024;
const MAX_INSTRUCTIONS_FILE: u64 = 8 * MAX_INSTRUCTED as u64;

/// `dir`'s recorded project instructions, none when there is no file.
fn read_instructions(dir: &Path) -> Result<Vec<Instructed>, String> {
    let path = dir.join(INSTRUCTIONS);
    let bytes = match read_bounded(&path, MAX_INSTRUCTIONS_FILE) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        read => read.map_err(|e| format!("{}: {e}", path.display()))?,
    };
    let text = std::str::from_utf8(&bytes).map_err(|_| format!("{}: not UTF-8", path.display()))?;
    let value = td_json::parse(text).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut read: Vec<Instructed> = Vec::new();
    for (at, item) in value
        .as_arr()
        .ok_or_else(|| format!("{}: not a list", path.display()))?
        .iter()
        .enumerate()
    {
        let one = Instructed::from_json(item, &read, at)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        read.push(one);
    }
    Ok(read)
}

/// The record as its file holds it: a worktree read at the commit an
/// earlier one was, the same read, names that one (`"same": <index>`)
/// rather than holding the text again, so the file's bound holds
/// however many worktrees start there.
fn instructions_json(all: &[Instructed]) -> Json {
    Json::Arr(
        all.iter()
            .enumerate()
            .map(|(at, one)| {
                let earlier = all
                    .iter()
                    .take(at)
                    .position(|other| other.base == one.base && other.read == one.read);
                let earlier_rules = all
                    .iter()
                    .take(at)
                    .position(|other| other.base == one.base && other.rules == one.rules);
                one.to_json(earlier, earlier_rules)
            })
            .collect(),
    )
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

/// Replaces `dir/name` whole and durably, private to the owner
/// (`td_fs::replace`): a reader sees the old bytes or the new, never part
/// of either.
pub(crate) fn replace(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    td_fs::replace(&dir.join(name), bytes, 0o600).map_err(|e| e.to_string())
}

/// A conversation's `meta` (DESIGN.md §6). The mode and parent are null
/// until the increments that give them values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Meta {
    pub id: Id,
    pub role: Role,
    pub title: String,
    pub created: u64,
    /// The human paused it (DESIGN.md §3): it starts no turn of its own
    /// until resumed. The log's `Pause` events are the record; this is
    /// the list's copy, absent and false in a meta written before.
    pub paused: bool,
    /// The model and effort the human chose for it, none being the
    /// configuration's: the list's copy of the log's last `Choice`, null
    /// in a meta written before.
    pub model: Option<String>,
    pub effort: Option<String>,
    /// What it works in (DESIGN.md §7), fixed when it is created; null
    /// for a conversation with none.
    pub workspace: Option<Workspace>,
    /// The human archived it (DESIGN.md §7): the window starts no
    /// process for it and routes no message to it until unarchived. Only
    /// the window writes it; absent and false in a meta written before.
    pub archived: bool,
    /// A repository workspace's repositories its process has checked
    /// out, which its instances then bind (DESIGN.md §7); only that
    /// process writes it, never from what a jail could have written.
    /// Absent and empty in a meta written before.
    pub prepared: Vec<PathBuf>,
    /// Its repository workspace went with its archive (DESIGN.md §7): its
    /// process asks for no store and refuses the workspace tools. Only
    /// the window writes it, and never clears it; absent and false in a
    /// meta written before.
    pub removed: bool,
    /// Each base's remote-tracking ref as this conversation's process
    /// last set it (DESIGN.md §7, Keeping current): what a base moving is
    /// told against. Only that process writes it; absent and empty in a
    /// meta written before.
    pub tracked: Vec<Tracked>,
}

/// A base's remote-tracking ref, set to commit `id` in the repository
/// of `remote`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Tracked {
    pub remote: String,
    pub base: String,
    pub id: String,
}

impl Meta {
    fn to_json(&self) -> Json {
        Json::Obj(vec![
            ("version".into(), Json::from(1u32)),
            ("id".into(), Json::Str(self.id.to_string())),
            ("role".into(), Json::Str(self.role.word().into())),
            ("title".into(), Json::Str(self.title.clone())),
            (
                "workspace".into(),
                self.workspace
                    .as_ref()
                    .map_or(Json::Null, Workspace::to_json),
            ),
            ("model".into(), or_null(&self.model)),
            ("mode".into(), Json::Null),
            ("parent".into(), Json::Null),
            ("created".into(), Json::from(self.created)),
            ("paused".into(), Json::Bool(self.paused)),
            ("effort".into(), or_null(&self.effort)),
            ("archived".into(), Json::Bool(self.archived)),
            ("removed".into(), Json::Bool(self.removed)),
            (
                "tracked".into(),
                Json::Arr(
                    self.tracked
                        .iter()
                        .map(|tracked| {
                            Json::Obj(vec![
                                ("remote".into(), Json::Str(tracked.remote.clone())),
                                ("base".into(), Json::Str(tracked.base.clone())),
                                ("id".into(), Json::Str(tracked.id.clone())),
                            ])
                        })
                        .collect(),
                ),
            ),
            (
                "prepared".into(),
                Json::Arr(
                    self.prepared
                        .iter()
                        .map(|path| Json::Str(path.display().to_string()))
                        .collect(),
                ),
            ),
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
            paused: match value.get("paused") {
                None => false,
                Some(paused) => paused.as_bool().ok_or("meta's paused is not a boolean")?,
            },
            model: optional_str(value, "model").ok_or("meta's model is not a string")?,
            effort: optional_str(value, "effort").ok_or("meta's effort is not a string")?,
            workspace: match value.get("workspace") {
                None | Some(Json::Null) => None,
                Some(workspace) => {
                    Some(Workspace::from_json(workspace).map_err(|e| format!("meta: {e}"))?)
                }
            },
            archived: match value.get("archived") {
                None => false,
                Some(archived) => archived
                    .as_bool()
                    .ok_or("meta's archived is not a boolean")?,
            },
            removed: match value.get("removed") {
                None => false,
                Some(removed) => removed.as_bool().ok_or("meta's removed is not a boolean")?,
            },
            tracked: match value.get("tracked") {
                None => Vec::new(),
                Some(tracked) => tracked
                    .as_arr()
                    .ok_or("meta's tracked is not a list")?
                    .iter()
                    .map(|item| {
                        let text = |name: &str| item.get(name).and_then(Json::as_str);
                        match (text("remote"), text("base"), text("id")) {
                            (Some(remote), Some(base), Some(id)) if crate::git::object_id(id) => {
                                Ok(Tracked {
                                    remote: remote.to_string(),
                                    base: base.to_string(),
                                    id: id.to_string(),
                                })
                            }
                            _ => Err("meta's tracked holds an entry that is not a remote, a base and a commit"),
                        }
                    })
                    .collect::<Result<_, _>>()?,
            },
            prepared: match value.get("prepared") {
                None => Vec::new(),
                Some(prepared) => prepared
                    .as_arr()
                    .ok_or("meta's prepared is not a list")?
                    .iter()
                    .map(|path| {
                        path.as_str()
                            .map(PathBuf::from)
                            .filter(|path| path.is_absolute())
                            .ok_or("meta's prepared names a path that is not absolute")
                    })
                    .collect::<Result<_, _>>()?,
            },
        })
    }
}

/// A string member, or null, as `None`.
fn or_null(value: &Option<String>) -> Json {
    value.clone().map_or(Json::Null, Json::Str)
}

/// A member that is a string, absent or null; none when it is anything
/// else.
fn optional_str(value: &Json, name: &str) -> Option<Option<String>> {
    match value.get(name) {
        None | Some(Json::Null) => Some(None),
        Some(Json::Str(text)) => Some(Some(text.clone())),
        Some(_) => None,
    }
}

/// `meta`'s text, refused past what `read_meta` reads, so a record that
/// grows never leaves a conversation that cannot be opened.
fn bounded(meta: &Meta) -> Result<String, String> {
    let text = meta.to_json().to_string();
    if text.len() as u64 > MAX_META {
        return Err(format!("meta would be past {MAX_META} bytes"));
    }
    Ok(text)
}

fn read_meta(dir: &Path) -> Result<Meta, String> {
    let bytes = read_named(&dir.join("meta"), MAX_META)?;
    let value = td_json::parse_slice(&bytes).map_err(|e| format!("meta: {e}"))?;
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
/// Recovery), or the human's compaction, of the log up to the event it
/// names (§14). A model request is the other effect, opened by its own
/// `Request` record, which carries what the request was.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Effect {
    Turn,
    Compact,
}

impl Effect {
    fn word(self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::Compact => "compact",
        }
    }
    fn parse(word: &str) -> Option<Self> {
        match word {
            "turn" => Some(Self::Turn),
            "compact" => Some(Self::Compact),
            _ => None,
        }
    }
}

/// What a compaction's summary is asked of and keeps (DESIGN.md §14).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Summarize {
    /// The first event of the view the summary request is given, the
    /// oldest before it left out to fit; 0 when none was.
    pub from: u64,
    /// The first event of the recent tail the view keeps after it.
    pub tail: u64,
    /// What the person asked the summary to keep, compacting by hand.
    pub focus: Option<String>,
}

/// What a model request was for (DESIGN.md §5, §13).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Purpose {
    /// The conversation's own exchange: the prefix and the log's messages.
    Turn,
    /// A title from `title_model` after the first exchange.
    Title,
    /// One of the classifier's stages on a pending action (DESIGN.md
    /// §11): Jev's, or the reasoning stage's, as its head's model says.
    Classify,
    /// A compaction's handoff summary (DESIGN.md §14), its body the
    /// view the compaction before it names and the summary prompt.
    Compact,
}

impl Purpose {
    pub fn word(self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::Title => "title",
            Self::Classify => "classify",
            Self::Compact => "compact",
        }
    }
    fn parse(word: &str) -> Option<Self> {
        match word {
            "turn" => Some(Self::Turn),
            "title" => Some(Self::Title),
            "classify" => Some(Self::Classify),
            "compact" => Some(Self::Compact),
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

/// One tool call an assistant message asked for, as its reply assembled
/// it and as every later request sends it back (DESIGN.md §5).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Call {
    pub id: String,
    pub name: String,
    /// The function's arguments as the model wrote them, kept whether or
    /// not they parse: a JSON text when whole.
    pub arguments: String,
}

impl Call {
    fn to_json(&self) -> Json {
        Json::Obj(vec![
            ("id".into(), Json::Str(self.id.clone())),
            ("name".into(), Json::Str(self.name.clone())),
            ("arguments".into(), Json::Str(self.arguments.clone())),
        ])
    }

    fn from_json(value: &Json) -> Result<Self, String> {
        let field = |name: &str| {
            value
                .get(name)
                .and_then(Json::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("a tool call with no {name}"))
        };
        Ok(Self {
            id: field("id")?,
            name: field("name")?,
            arguments: field("arguments")?,
        })
    }
}

/// A todo item's state (DESIGN.md §12).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Pending,
    InProgress,
    Done,
    Cancelled,
}

impl Status {
    pub const ALL: [Self; 4] = [Self::Pending, Self::InProgress, Self::Done, Self::Cancelled];

    pub fn word(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
        }
    }
    pub fn parse(word: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.word() == word)
    }
}

/// One item of a todo list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TodoItem {
    pub content: String,
    pub status: Status,
}

/// Why a message from another conversation started no turn (DESIGN.md
/// §3): its receiver was paused, or had spent its wake budget.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Held {
    Paused,
    Budget,
}

impl Held {
    pub fn word(self) -> &'static str {
        match self {
            Self::Paused => "paused",
            Self::Budget => "budget",
        }
    }
    fn parse(word: &str) -> Option<Self> {
        match word {
            "paused" => Some(Self::Paused),
            "budget" => Some(Self::Budget),
            _ => None,
        }
    }
}

/// What a restart tells the model of a tool call it found started and
/// not finished (DESIGN.md §6, Recovery).
pub const CALL_INTERRUPTED: &str = "interrupted: td-agent restarted while this call ran, so whether it had any effect is unknown; check the state before relying on it";
/// What a tool call that never ran is answered with, so that every call
/// a reply asked for has its one result.
pub const CALL_NOT_RUN: &str = "not run: the turn ended before this call ran";
/// How a background process still running when its conversation's
/// process stopped ended, as far as the log knows (DESIGN.md §12).
pub const PROCESS_LOST: &str = "lost: td-agent stopped while it ran";

/// A background process as the log records it (DESIGN.md §12).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Background {
    /// `p1` is 1.
    pub number: u64,
    pub command: String,
    /// When it started, as the log's time.
    pub started: u64,
    /// How it ended, if it has.
    pub ended: Option<String>,
}

/// The background processes `events` record, in the order they started.
pub fn backgrounds(events: &[Event]) -> Vec<Background> {
    let mut out: Vec<Background> = Vec::new();
    for event in events {
        match &event.kind {
            Kind::Process {
                number, command, ..
            } => out.push(Background {
                number: *number,
                command: command.clone(),
                started: event.time,
                ended: None,
            }),
            Kind::Ended { number, how, .. } => {
                if let Some(one) = out
                    .iter_mut()
                    .rev()
                    .find(|one| one.number == *number && one.ended.is_none())
                {
                    one.ended = Some(how.clone());
                }
            }
            _ => {}
        }
    }
    out
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
    /// td-agent's own news of the conversation's workspace, a worktree
    /// ready or failed or a base that moved upstream (DESIGN.md §3,
    /// Notifications): shown as a notice is, and given to the model too.
    Notification { text: String },
    /// A request prefix (DESIGN.md §6, §13) superseding the `prefix` file
    /// and any earlier one from here on, as exact text: a JSON object of
    /// the tools and the messages every request of the conversation begins
    /// with, its `messages` last, or, as increment 7 wrote it, an array of
    /// the messages alone (`client::turn_body`).
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
    /// interrupted had brought: kept and shown, never sent back, its tool
    /// calls never run.
    Assistant {
        request: u64,
        content: Option<String>,
        reasoning: Option<String>,
        details: Option<String>,
        finish: String,
        incomplete: bool,
        /// The tool calls it asked for, in their order.
        calls: Vec<Call>,
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
    /// A message from another conversation (DESIGN.md §3), with the
    /// window's delivery id, which a receiver logs once: its sender and
    /// the sender's role, its text, the status a `report` gave it, and
    /// why it started no turn, when it did not.
    Message {
        delivery: String,
        from: Id,
        role: Role,
        text: String,
        status: Option<String>,
        held: Option<Held>,
    },
    /// Tool call `id` of the assistant message at `reply` started: logged
    /// and synced before it runs (DESIGN.md §6, Recovery).
    ToolCall {
        reply: u64,
        id: String,
        name: String,
    },
    /// A tool call's result as returned to the model, whole: the
    /// `ToolCall` record it finishes (0 for a call that never ran),
    /// whether it is a refusal or a failure rather than the tool's answer,
    /// what the log keeps beyond it, a command's output's head and tail
    /// (DESIGN.md §12), and the digest of the file a read or write left,
    /// which the next replacement of it expects; neither is sent to the
    /// model.
    ToolResult {
        reply: u64,
        id: String,
        name: String,
        call: u64,
        content: String,
        error: bool,
        kept: Option<String>,
        digest: Option<String>,
        /// A patch's files, each with its digest now or none when it was
        /// deleted or moved away; empty for every other call.
        digests: Vec<(String, Option<String>)>,
    },
    /// A compaction (DESIGN.md §14): the tool results it pruned, sent as
    /// stubs from here on, and the summary it asks for, if any, of the
    /// `compact` request that follows it.
    Compaction {
        pruned: Vec<u64>,
        summary: Option<Summarize>,
    },
    /// The todo list as written, whole (DESIGN.md §12); `cleared` when the
    /// human cleared it from the window.
    Todo { items: Vec<TodoItem>, cleared: bool },
    /// A record of a kind td-agent no longer makes, named by its `kind`:
    /// the step snapshots (`snapshot`) and their undo and redo (`undo`,
    /// `redo`) an older td-agent logged, read so that its logs open, and
    /// otherwise nothing but `trees`: a snapshot's worktrees, each its
    /// checkout and the tree after its step, which a compaction made
    /// then carried, so that its carried text is rebuilt byte for byte
    /// (DESIGN.md §6, §14).
    Retired {
        kind: String,
        trees: Vec<(String, String)>,
    },
    /// Background process `number` (`p1` is 1) started, by the `ToolCall`
    /// at `call`, running `command` (DESIGN.md §12).
    Process {
        number: u64,
        call: u64,
        command: String,
    },
    /// Background process `number` ended: `how`, as `process_list` says
    /// it, an exit status, killed, timed out, failed or lost; the tail of
    /// its output, made visible, when its conversation's process heard
    /// it end; and why it started no turn, when it would have (DESIGN.md
    /// §3, §12). The model is told it as a notification.
    Ended {
        number: u64,
        how: String,
        tail: Option<String>,
        held: Option<Held>,
    },
    /// The human paused or resumed the conversation (DESIGN.md §3).
    Pause { paused: bool },
    /// The human chose the conversation's model and reasoning effort
    /// (DESIGN.md §4, §5), whole: none is the configuration's.
    Choice {
        model: Option<String>,
        effort: Option<String>,
    },
    /// An approval decision (DESIGN.md §6, §11): the `ToolCall` it
    /// decided, its outcome, who decided it (a rule, a classifier stage or
    /// the human), Jev's probabilities where it gave them, and the reason.
    /// The human's decisions on cards write one (increment 10), rules and
    /// the classifier later (increment 13); the history tools show a model
    /// only its outcome and who decided.
    Approval {
        call: u64,
        outcome: String,
        by: String,
        probabilities: Option<String>,
        reason: Option<String>,
    },
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
            Kind::Notification { text } => {
                put("kind", Json::Str("notification".into()));
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
                calls,
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
                if !calls.is_empty() {
                    put(
                        "tool_calls",
                        Json::Arr(calls.iter().map(Call::to_json).collect()),
                    );
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
            Kind::Message {
                delivery,
                from,
                role,
                text,
                status,
                held,
            } => {
                put("kind", Json::Str("message".into()));
                put("delivery", Json::Str(delivery.clone()));
                put("from", Json::Str(from.to_string()));
                put("role", Json::Str(role.word().into()));
                put("text", Json::Str(text.clone()));
                if let Some(status) = status {
                    put("status", Json::Str(status.clone()));
                }
                if let Some(held) = held {
                    put("held", Json::Str(held.word().into()));
                }
            }
            Kind::ToolCall { reply, id, name } => {
                put("kind", Json::Str("tool_call".into()));
                put("reply", Json::from(*reply));
                put("id", Json::Str(id.clone()));
                put("name", Json::Str(name.clone()));
            }
            Kind::ToolResult {
                reply,
                id,
                name,
                call,
                content,
                error,
                kept,
                digest,
                digests,
            } => {
                put("kind", Json::Str("tool_result".into()));
                put("reply", Json::from(*reply));
                put("id", Json::Str(id.clone()));
                put("name", Json::Str(name.clone()));
                put("call", Json::from(*call));
                put("content", Json::Str(content.clone()));
                if *error {
                    put("error", Json::Bool(true));
                }
                if let Some(kept) = kept {
                    put("kept", Json::Str(kept.clone()));
                }
                if let Some(digest) = digest {
                    put("digest", Json::Str(digest.clone()));
                }
                if !digests.is_empty() {
                    put(
                        "digests",
                        Json::Arr(
                            digests
                                .iter()
                                .map(|(path, digest)| {
                                    Json::Arr(vec![
                                        Json::Str(path.clone()),
                                        digest.clone().map_or(Json::Null, Json::Str),
                                    ])
                                })
                                .collect(),
                        ),
                    );
                }
            }
            Kind::Todo { items, cleared } => {
                put("kind", Json::Str("todo".into()));
                let items = items
                    .iter()
                    .map(|item| {
                        Json::Obj(vec![
                            ("content".into(), Json::Str(item.content.clone())),
                            ("status".into(), Json::Str(item.status.word().into())),
                        ])
                    })
                    .collect();
                put("items", Json::Arr(items));
                if *cleared {
                    put("cleared", Json::Bool(true));
                }
            }
            Kind::Retired { kind, trees } => {
                put("kind", Json::Str(kind.clone()));
                if !trees.is_empty() {
                    let trees = trees
                        .iter()
                        .map(|(checkout, after)| {
                            Json::Obj(vec![
                                ("checkout".into(), Json::Str(checkout.clone())),
                                ("after".into(), Json::Str(after.clone())),
                            ])
                        })
                        .collect();
                    put("worktrees", Json::Arr(trees));
                }
            }
            Kind::Process {
                number,
                call,
                command,
            } => {
                put("kind", Json::Str("process".into()));
                put("number", Json::from(*number));
                put("call", Json::from(*call));
                put("command", Json::Str(command.clone()));
            }
            Kind::Ended {
                number,
                how,
                tail,
                held,
            } => {
                put("kind", Json::Str("ended".into()));
                put("number", Json::from(*number));
                put("how", Json::Str(how.clone()));
                if let Some(tail) = tail {
                    put("tail", Json::Str(tail.clone()));
                }
                if let Some(held) = held {
                    put("held", Json::Str(held.word().into()));
                }
            }
            Kind::Compaction { pruned, summary } => {
                put("kind", Json::Str("compaction".into()));
                put(
                    "pruned",
                    Json::Arr(pruned.iter().map(|n| Json::from(*n)).collect()),
                );
                if let Some(summary) = summary {
                    let mut asked = vec![
                        ("from".into(), Json::from(summary.from)),
                        ("tail".into(), Json::from(summary.tail)),
                    ];
                    if let Some(focus) = &summary.focus {
                        asked.push(("focus".into(), Json::Str(focus.clone())));
                    }
                    put("summary", Json::Obj(asked));
                }
            }
            Kind::Pause { paused } => {
                put("kind", Json::Str("pause".into()));
                put("paused", Json::Bool(*paused));
            }
            Kind::Choice { model, effort } => {
                put("kind", Json::Str("choice".into()));
                put("model", or_null(model));
                put("effort", or_null(effort));
            }
            Kind::Approval {
                call,
                outcome,
                by,
                probabilities,
                reason,
            } => {
                put("kind", Json::Str("approval".into()));
                put("call", Json::from(*call));
                put("outcome", Json::Str(outcome.clone()));
                put("by", Json::Str(by.clone()));
                if let Some(probabilities) = probabilities {
                    put("probabilities", Json::Str(probabilities.clone()));
                }
                if let Some(reason) = reason {
                    put("reason", Json::Str(reason.clone()));
                }
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
        let optional = |name: &str| -> Result<Option<String>, String> {
            match value.get(name) {
                None | Some(Json::Null) => Ok(None),
                Some(Json::Str(text)) => Ok(Some(text.clone())),
                Some(_) => Err(format!("{name} is not a string")),
            }
        };
        let flag = |name: &str| -> Result<bool, String> {
            match value.get(name) {
                None => Ok(false),
                Some(flag) => flag
                    .as_bool()
                    .ok_or_else(|| format!("{name} is not a boolean")),
            }
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
            Some("notification") => Kind::Notification {
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
            Some("assistant") => Kind::Assistant {
                request: number("request")?,
                content: optional("content")?,
                reasoning: optional("reasoning")?,
                details: optional("reasoning_details")?,
                finish: string("finish")?,
                incomplete: flag("incomplete")?,
                calls: match value.get("tool_calls") {
                    None => Vec::new(),
                    Some(calls) => calls
                        .as_arr()
                        .ok_or("tool_calls is not a list")?
                        .iter()
                        .map(Call::from_json)
                        .collect::<Result<_, _>>()?,
                },
            },
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
            Some("message") => Kind::Message {
                delivery: string("delivery")?,
                from: Id::parse(&string("from")?).ok_or("from is not an id")?,
                role: Role::parse(&string("role")?).ok_or("no role")?,
                text: string("text")?,
                status: optional("status")?,
                held: match optional("held")? {
                    None => None,
                    Some(word) => Some(Held::parse(&word).ok_or("held is not a reason")?),
                },
            },
            Some("tool_call") => Kind::ToolCall {
                reply: number("reply")?,
                id: string("id")?,
                name: string("name")?,
            },
            Some("tool_result") => Kind::ToolResult {
                reply: number("reply")?,
                id: string("id")?,
                name: string("name")?,
                call: number("call")?,
                content: string("content")?,
                error: flag("error")?,
                kept: optional("kept")?,
                digest: optional("digest")?,
                // A log from before patches holds none.
                digests: match value.get("digests") {
                    None => Vec::new(),
                    Some(list) => list
                        .as_arr()
                        .filter(|items| items.len() <= crate::patch::MAX_FILES.saturating_mul(2))
                        .ok_or("a tool result's `digests` is not a bounded list")?
                        .iter()
                        .map(|item| match item.as_arr() {
                            Some([Json::Str(path), Json::Str(digest)]) => {
                                Ok((path.clone(), Some(digest.clone())))
                            }
                            Some([Json::Str(path), Json::Null]) => Ok((path.clone(), None)),
                            _ => Err("a tool result's `digests` holds something not a [path, digest] pair"),
                        })
                        .collect::<Result<_, _>>()?,
                },
            },
            Some("todo") => Kind::Todo {
                items: value
                    .get("items")
                    .and_then(Json::as_arr)
                    .ok_or("no items")?
                    .iter()
                    .map(|item| {
                        Ok(TodoItem {
                            content: item
                                .get("content")
                                .and_then(Json::as_str)
                                .ok_or("an item with no content")?
                                .to_string(),
                            status: item
                                .get("status")
                                .and_then(Json::as_str)
                                .and_then(Status::parse)
                                .ok_or("an item with no status")?,
                        })
                    })
                    .collect::<Result<_, String>>()?,
                cleared: flag("cleared")?,
            },
            Some("process") => Kind::Process {
                number: number("number")?,
                call: number("call")?,
                command: string("command")?,
            },
            Some("ended") => Kind::Ended {
                number: number("number")?,
                how: string("how")?,
                tail: optional("tail")?,
                held: match optional("held")? {
                    None => None,
                    Some(word) => Some(Held::parse(&word).ok_or("an unknown hold")?),
                },
            },
            Some(retired @ ("snapshot" | "undo" | "redo")) => Kind::Retired {
                kind: retired.into(),
                // Each worktree's checkout and tree after, where both are
                // text; nothing else of it is read.
                trees: value
                    .get("worktrees")
                    .and_then(Json::as_arr)
                    .map(|all| {
                        all.iter()
                            .filter_map(|one| {
                                let text = |name: &str| one.get(name).and_then(Json::as_str);
                                Some((text("checkout")?.to_string(), text("after")?.to_string()))
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            },
            Some("compaction") => Kind::Compaction {
                pruned: value
                    .get("pruned")
                    .and_then(Json::as_arr)
                    .ok_or("no pruned list")?
                    .iter()
                    .map(|n| n.as_u64().ok_or("a pruned result that is no number"))
                    .collect::<Result<_, _>>()?,
                summary: match value.get("summary") {
                    None => None,
                    Some(asked) => Some(Summarize {
                        from: asked
                            .get("from")
                            .and_then(Json::as_u64)
                            .ok_or("a summary from no event")?,
                        tail: asked
                            .get("tail")
                            .and_then(Json::as_u64)
                            .ok_or("a summary with no tail")?,
                        focus: match asked.get("focus") {
                            None => None,
                            Some(focus) => {
                                Some(focus.as_str().ok_or("a focus that is no text")?.to_string())
                            }
                        },
                    }),
                },
            },
            Some("pause") => Kind::Pause {
                paused: value
                    .get("paused")
                    .and_then(Json::as_bool)
                    .ok_or("no paused")?,
            },
            Some("choice") => Kind::Choice {
                model: optional("model")?,
                effort: optional("effort")?,
            },
            Some("approval") => Kind::Approval {
                call: number("call")?,
                outcome: string("outcome")?,
                by: string("by")?,
                probabilities: optional("probabilities")?,
                reason: optional("reason")?,
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
    /// Its worktrees' project instructions, as recorded (`instructed`).
    instructions: Vec<Instructed>,
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
        Self::open_as(state, id, create.map(|role| (role, None)), wait)
    }

    /// Creates conversation `id` as `role`, working in `workspace`, and
    /// opens it, as `open` does.
    pub fn create(
        state: &StateDir,
        id: &Id,
        role: Role,
        workspace: Option<Workspace>,
        wait: Duration,
    ) -> Result<(Self, Load), String> {
        Self::open_as(state, id, Some((role, workspace)), wait)
    }

    fn open_as(
        state: &StateDir,
        id: &Id,
        create: Option<(Role, Option<Workspace>)>,
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
            Some((role, workspace)) => {
                let meta = Meta {
                    id: id.clone(),
                    role,
                    title: role.first_title().to_string(),
                    created: now(),
                    paused: false,
                    model: None,
                    effort: None,
                    workspace,
                    archived: false,
                    prepared: Vec::new(),
                    removed: false,
                    tracked: Vec::new(),
                };
                // The request prefix (§13) is written once and never
                // rewritten; a later prefix is a log event.
                create_file(
                    &dir.join("prefix"),
                    crate::prompt::prefix(meta.created).as_bytes(),
                )?;
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
        let instructions = read_instructions(&dir)?;
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
            instructions,
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
        let mut repaired = false;
        for of in conversation.unturned() {
            conversation.append(Kind::Started {
                effect: Effect::Turn,
                of,
            })?;
        }
        // Every call a whole reply asked for has its one result before any
        // later request: one found started is interrupted, its effect
        // unknown, and one never started was not run. Neither runs again.
        for (reply, id, name, call) in conversation.unanswered() {
            let content = if call == 0 {
                CALL_NOT_RUN
            } else {
                CALL_INTERRUPTED
            };
            conversation.append(Kind::ToolResult {
                reply,
                id,
                name,
                call,
                content: content.into(),
                error: true,
                kept: None,
                digest: None,
                digests: Vec::new(),
            })?;
            if call != 0 {
                load.interrupted.push(call);
            }
            repaired = true;
        }
        for started in conversation.unfinished() {
            conversation.append(Kind::Interrupted { started })?;
            load.interrupted.push(started);
        }
        // No background process outlives the process that ran it.
        let running: Vec<u64> = backgrounds(&conversation.events)
            .into_iter()
            .filter(|one| one.ended.is_none())
            .map(|one| one.number)
            .collect();
        for number in running {
            conversation.append(Kind::Ended {
                number,
                how: PROCESS_LOST.into(),
                tail: None,
                held: None,
            })?;
            repaired = true;
        }
        if repaired || load.torn.is_some() || !load.interrupted.is_empty() {
            conversation.sync()?;
        }
        // The log is the record of pausing; `meta` follows it, and a
        // process that died between the two is put right here.
        let paused = conversation.events.iter().rev().find_map(|e| match e.kind {
            Kind::Pause { paused } => Some(paused),
            _ => None,
        });
        if let Some(paused) = paused.filter(|p| *p != conversation.meta.paused) {
            conversation.set_paused(paused)?;
        }
        // So is the human's choice of model and effort.
        let chosen = conversation.choice();
        if chosen
            != (
                conversation.meta.model.clone(),
                conversation.meta.effort.clone(),
            )
        {
            conversation.set_choice(chosen.0, chosen.1)?;
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

    /// Whether a message with this delivery id, the human's or another
    /// conversation's, is already logged.
    pub fn delivered(&self, delivery: &str) -> bool {
        self.events.iter().any(|e| match &e.kind {
            Kind::User { delivery: d, .. } | Kind::Message { delivery: d, .. } => d == delivery,
            _ => false,
        })
    }

    /// Every message that should have started a turn and has none: the
    /// human's, and another conversation's that was not held.
    fn unturned(&self) -> Vec<u64> {
        let mut users = Vec::new();
        // The turns under way: a person's message logged during one was
        // taken into it at a step (DESIGN.md §2) and needs no turn.
        let mut running: Vec<u64> = Vec::new();
        for event in &self.events {
            match &event.kind {
                Kind::User { .. } if !running.is_empty() => {}
                Kind::User { .. } | Kind::Message { held: None, .. } => users.push(event.seq),
                Kind::Started {
                    effect: Effect::Turn,
                    of,
                } => {
                    users.retain(|seq| seq != of);
                    running.push(event.seq);
                }
                Kind::Finished { started, .. } | Kind::Interrupted { started } => {
                    running.retain(|seq| seq != started)
                }
                _ => {}
            }
        }
        users
    }

    /// Every tool call a whole reply asked for that has no result yet: its
    /// reply, id and name, and its `ToolCall` record, 0 when it never
    /// started.
    fn unanswered(&self) -> Vec<(u64, String, String, u64)> {
        let mut open: Vec<(u64, String, String, u64)> = Vec::new();
        for event in &self.events {
            match &event.kind {
                Kind::Assistant {
                    incomplete: false,
                    calls,
                    ..
                } => open.extend(
                    calls
                        .iter()
                        .map(|c| (event.seq, c.id.clone(), c.name.clone(), 0)),
                ),
                Kind::ToolCall { reply, id, .. } => {
                    if let Some(entry) = open.iter_mut().find(|(r, i, _, _)| r == reply && i == id)
                    {
                        entry.3 = event.seq;
                    }
                }
                Kind::ToolResult { reply, id, .. } => {
                    open.retain(|(r, i, _, _)| !(r == reply && i == id))
                }
                _ => {}
            }
        }
        open
    }

    /// Its worktrees' project instructions as recorded, in the order
    /// they came (DESIGN.md §13).
    pub fn instructions(&self) -> &[Instructed] {
        &self.instructions
    }

    /// Records `more`, a worktree's replacing what it had: only a
    /// repository not yet prepared is read again (DESIGN.md §7), so the
    /// record is the commit its worktree was checked out at, and holds
    /// once it is. Past `MAX_INSTRUCTED` bytes in all, a commit's text
    /// counted once, a later worktree's is recorded unread, saying so;
    /// past `rules::MAX_CARRIED` bytes of rules, every worktree's
    /// counted, a later worktree's rules are too, so it asks.
    pub fn instructed(&mut self, more: Vec<Instructed>) -> Result<(), String> {
        // What stays is held as it was: only the new reads are bounded,
        // against the rest, so no other worktree's record changes.
        let kept: Vec<&Instructed> = self
            .instructions
            .iter()
            .filter(|done| more.iter().all(|one| one.checkout != done.checkout))
            .collect();
        let mut counted: Vec<(&str, &crate::repo::Instructions)> = Vec::new();
        let mut carried = 0usize;
        for done in &kept {
            if !counted.contains(&(done.base.as_str(), &done.read)) {
                carried = carried.saturating_add(done.read.carried());
                counted.push((done.base.as_str(), &done.read));
            }
        }
        // A commit's rules count once, as its instructions do.
        let mut ruled: Vec<(String, crate::rules::Read)> = Vec::new();
        let mut rules_carried = 0usize;
        for done in &kept {
            if !ruled
                .iter()
                .any(|(base, rules)| *base == done.base && *rules == done.rules)
            {
                rules_carried = rules_carried.saturating_add(done.rules.carried());
                ruled.push((done.base.clone(), done.rules.clone()));
            }
        }
        let mut bounded: Vec<Instructed> = Vec::new();
        for mut one in more {
            if !ruled
                .iter()
                .any(|(base, rules)| *base == one.base && *rules == one.rules)
            {
                if rules_carried.saturating_add(one.rules.carried()) > crate::rules::MAX_CARRIED {
                    one.rules = crate::rules::Read::unread(&format!(
                        "{} is past {} bytes with the other worktrees' rules",
                        crate::rules::FILE,
                        crate::rules::MAX_CARRIED
                    ));
                }
                rules_carried = rules_carried.saturating_add(one.rules.carried());
                ruled.push((one.base.clone(), one.rules.clone()));
            }
            let seen = counted
                .iter()
                .any(|(base, read)| *base == one.base && **read == one.read)
                || bounded
                    .iter()
                    .any(|done| done.base == one.base && done.read == one.read);
            if !seen {
                if carried.saturating_add(one.read.carried()) > MAX_INSTRUCTED {
                    one.read = crate::repo::Instructions::Unread {
                        why: format!(
                            "past {MAX_INSTRUCTED} bytes with the other worktrees' instructions"
                        ),
                    };
                }
                carried = carried.saturating_add(one.read.carried());
            }
            bounded.push(one);
        }
        let mut all = self.instructions.clone();
        for one in bounded {
            match all.iter_mut().find(|done| done.checkout == one.checkout) {
                Some(done) => *done = one,
                None => all.push(one),
            }
        }
        if all == self.instructions {
            return Ok(());
        }
        let text = instructions_json(&all).to_string();
        replace(&self.dir, INSTRUCTIONS, text.as_bytes())?;
        self.instructions = all;
        Ok(())
    }

    /// Records repository `repository` checked out in `meta`, once.
    pub fn set_prepared(&mut self, repository: &Path) -> Result<(), String> {
        if self.meta.prepared.iter().any(|done| done == repository) {
            return Ok(());
        }
        let mut meta = self.meta.clone();
        meta.prepared.push(repository.to_path_buf());
        replace(&self.dir, "meta", bounded(&meta)?.as_bytes())?;
        self.meta = meta;
        Ok(())
    }

    /// Records `remote`'s `heads`, each a base and the commit its
    /// remote-tracking ref was set to, in `meta`, in place of what was
    /// recorded of those bases.
    pub fn set_tracked(&mut self, remote: &str, heads: &[(String, String)]) -> Result<(), String> {
        let mut meta = self.meta.clone();
        meta.tracked.retain(|tracked| {
            tracked.remote != remote || !heads.iter().any(|(base, _)| *base == tracked.base)
        });
        meta.tracked.extend(heads.iter().map(|(base, id)| Tracked {
            remote: remote.to_string(),
            base: base.clone(),
            id: id.clone(),
        }));
        replace(&self.dir, "meta", bounded(&meta)?.as_bytes())?;
        self.meta = meta;
        Ok(())
    }

    /// Marks the conversation paused or resumed in its `meta`, the list's
    /// copy of the log's `Pause` events.
    pub fn set_paused(&mut self, paused: bool) -> Result<(), String> {
        let mut meta = self.meta.clone();
        meta.paused = paused;
        replace(&self.dir, "meta", meta.to_json().to_string().as_bytes())?;
        self.meta = meta;
        Ok(())
    }

    /// The model and effort the log's last `Choice` holds, none being the
    /// configuration's.
    pub fn choice(&self) -> (Option<String>, Option<String>) {
        self.events
            .iter()
            .rev()
            .find_map(|e| match &e.kind {
                Kind::Choice { model, effort } => Some((model.clone(), effort.clone())),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Records the human's choice in `meta`, the list's copy of the log's
    /// `Choice` events.
    pub fn set_choice(
        &mut self,
        model: Option<String>,
        effort: Option<String>,
    ) -> Result<(), String> {
        let mut meta = self.meta.clone();
        meta.model = model;
        meta.effort = effort;
        replace(&self.dir, "meta", meta.to_json().to_string().as_bytes())?;
        self.meta = meta;
        Ok(())
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

/// Conversation `id`'s log as the store holds it now, read by a process
/// that is not its writer: a final line still being written is left out.
pub fn read_log(state: &StateDir, id: &Id) -> Result<Vec<Event>, String> {
    let path = state.conversation(id).join("log");
    let bytes = read_bounded(&path, MAX_LOG).map_err(|e| format!("{}: {e}", path.display()))?;
    parse_log(&bytes)
        .map(|(events, _)| events)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// The `prefix` file of conversation `id`, read without opening the
/// conversation, as `read_log` reads its log.
pub fn read_prefix(state: &StateDir, id: &Id) -> Result<String, String> {
    let path = state.conversation(id).join("prefix");
    let bytes = read_bounded(&path, MAX_PREFIX).map_err(|e| format!("{}: {e}", path.display()))?;
    String::from_utf8(bytes).map_err(|_| format!("{}: not UTF-8", path.display()))
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
        let value = td_json::parse_slice(body)
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
    fn a_prefix_file_is_read_bounded_and_never_through_a_link() {
        let scratch = Scratch::new("prefix");
        let state = scratch.state();
        let id = Id::random().unwrap();
        assert!(read_prefix(&state, &id).is_err(), "no conversation");
        let dir = state.conversation(&id);
        DirBuilder::new().mode(0o700).create(&dir).unwrap();
        std::fs::write(dir.join("prefix"), "{}").unwrap();
        assert_eq!(read_prefix(&state, &id).unwrap(), "{}");
        std::fs::write(dir.join("prefix"), [0xff]).unwrap();
        assert!(read_prefix(&state, &id).unwrap_err().contains("not UTF-8"));
        std::fs::remove_file(dir.join("prefix")).unwrap();
        std::fs::write(scratch.0.join("elsewhere"), "{}").unwrap();
        std::os::unix::fs::symlink(scratch.0.join("elsewhere"), dir.join("prefix")).unwrap();
        assert!(read_prefix(&state, &id).is_err(), "followed a link");
        std::fs::remove_file(dir.join("prefix")).unwrap();
        std::fs::write(dir.join("prefix"), vec![b' '; MAX_PREFIX as usize + 1]).unwrap();
        assert!(read_prefix(&state, &id).is_err(), "past the bound");
    }

    /// A conversation is deleted only once its writer is gone; a deletion
    /// a crash cut short is finished by the next listing.
    #[test]
    fn a_conversation_is_deleted_whole_and_a_cut_deletion_finished() {
        let scratch = Scratch::new("delete");
        let state = scratch.state();
        let id = Id::random().unwrap();
        let kept = Id::random().unwrap();
        let (conversation, _) =
            Conversation::open(&state, &id, Some(Role::Conversation), LOCK_WAIT).unwrap();
        Conversation::open(&state, &kept, Some(Role::Conversation), LOCK_WAIT).unwrap();
        // Its writer holds the lock: refused, and nothing moved.
        let refused = state.delete(&id).unwrap_err();
        assert!(refused.contains("already has its writer"), "{refused}");
        assert!(state.conversation(&id).exists());
        drop(conversation);
        assert_eq!(state.delete(&id).unwrap(), None);
        assert!(!state.conversation(&id).exists());
        let listed: Vec<Id> = state.list().0.into_iter().map(|m| m.id).collect();
        assert_eq!(listed, std::slice::from_ref(&kept));
        // One never made, or gone, is deleted already.
        assert_eq!(state.delete(&id).unwrap(), None);
        // A deletion cut short is out of the list; one under way holds
        // its lock, and the window's sweep leaves it.
        let cut = state.conversations().join(format!("{DELETING}{kept}"));
        std::fs::rename(state.conversation(&kept), &cut).unwrap();
        let (metas, problems) = state.list();
        assert!(metas.is_empty() && problems.is_empty(), "{problems:?}");
        assert!(cut.exists(), "a listing removes nothing");
        let held = lock_conversation(&cut, Duration::ZERO).unwrap();
        assert!(state.sweep_deleted().is_empty());
        assert!(cut.exists());
        // One a crash cut short is finished, by a later sweep should a
        // child another test forked while it was held not yet have
        // execed.
        drop(held);
        let deadline = Instant::now() + LOCK_WAIT;
        while cut.exists() && Instant::now() < deadline {
            assert!(state.sweep_deleted().is_empty());
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!cut.exists());
    }

    /// A workspace is fixed in the meta at creation, and a conversation's
    /// jail directory goes with it, or with the window's next sweep.
    #[test]
    fn a_workspace_is_recorded_and_its_jail_directory_deleted_with_it() {
        let scratch = Scratch::new("workspace");
        let state = scratch.state();
        let id = Id::random().unwrap();
        let workspace = Workspace::Directory("/home/u/notes".into());
        let (conversation, _) = Conversation::create(
            &state,
            &id,
            Role::Conversation,
            Some(workspace.clone()),
            LOCK_WAIT,
        )
        .unwrap();
        assert_eq!(conversation.meta().workspace, Some(workspace.clone()));
        drop(conversation);
        let (conversation, _) = Conversation::open(&state, &id, None, LOCK_WAIT).unwrap();
        assert_eq!(conversation.meta().workspace, Some(workspace));
        drop(conversation);
        let (metas, _) = state.list();
        assert_eq!(
            metas
                .first()
                .and_then(|meta| meta.workspace.as_ref())
                .map(Workspace::label)
                .as_deref(),
            Some("/home/u/notes")
        );
        let jail = crate::workspace::jail_dir(&state, &id);
        std::fs::create_dir_all(jail.join("scratch/work")).unwrap();
        assert_eq!(state.delete(&id).unwrap(), None);
        assert!(!jail.exists());
        // Removed on its thread; one the window closed on is swept.
        let doomed = state.root().join("jail").join(format!("{DELETING}{id}"));
        let deadline = Instant::now() + Duration::from_secs(10);
        while doomed.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!doomed.exists());
        std::fs::create_dir_all(doomed.join("scratch")).unwrap();
        // One left without its conversation is the sweep's; one with its
        // conversation is kept.
        let kept = Id::random().unwrap();
        drop(Conversation::open(&state, &kept, Some(Role::Conversation), LOCK_WAIT).unwrap());
        std::fs::create_dir_all(jail.join("home")).unwrap();
        let held = crate::workspace::jail_dir(&state, &kept);
        std::fs::create_dir_all(&held).unwrap();
        assert!(state.sweep_deleted().is_empty());
        let deadline = Instant::now() + Duration::from_secs(10);
        while (jail.exists() || doomed.exists()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!jail.exists() && !doomed.exists());
        assert!(held.exists());
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
            crate::prompt::prefix(read_meta(&dir).unwrap().created)
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

    /// A compaction names an event, but is no turn of it: a message whose
    /// only start is a compaction's is given its turn on reopening.
    #[test]
    fn a_compaction_is_no_turn_of_the_message_it_names() {
        let scratch = Scratch::new("compact-unturned");
        let state = scratch.state();
        let id = Id::random().unwrap();
        {
            let (mut conversation, _) =
                Conversation::open(&state, &id, Some(Role::Conversation), LOCK_WAIT).unwrap();
            conversation.append(user("kept")).unwrap();
            conversation
                .append(Kind::Started {
                    effect: Effect::Compact,
                    of: 1,
                })
                .unwrap();
            conversation
                .append(Kind::Finished {
                    started: 2,
                    outcome: "compacted".into(),
                    retry: false,
                })
                .unwrap();
            conversation.sync().unwrap();
        }
        let (conversation, load) = Conversation::open(&state, &id, None, LOCK_WAIT).unwrap();
        assert!(
            conversation.events().iter().any(|e| matches!(
                e.kind,
                Kind::Started {
                    effect: Effect::Turn,
                    of: 1
                }
            )),
            "{:?}",
            conversation.events()
        );
        assert_eq!(load.interrupted, [4]);
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

    /// No background process outlives its conversation's process: one
    /// the log has running when it opens is recorded lost, once.
    #[test]
    fn a_background_process_still_running_at_open_is_lost() {
        let scratch = Scratch::new("lost");
        let state = scratch.state();
        let id = Id::random().unwrap();
        {
            let (mut conversation, _) =
                Conversation::open(&state, &id, Some(Role::Conversation), LOCK_WAIT).unwrap();
            for (number, command) in [(1, "make"), (2, "watch"), (3, "serve")] {
                conversation
                    .append(Kind::Process {
                        number,
                        call: 0,
                        command: command.into(),
                    })
                    .unwrap();
            }
            conversation
                .append(Kind::Ended {
                    number: 2,
                    how: "killed".into(),
                    tail: None,
                    held: None,
                })
                .unwrap();
            // A number the log holds twice, as only a hand could write
            // it: an end is the latest unended one's.
            conversation
                .append(Kind::Process {
                    number: 2,
                    call: 0,
                    command: "again".into(),
                })
                .unwrap();
            conversation.sync().unwrap();
        }
        let (conversation, _) = Conversation::open(&state, &id, None, LOCK_WAIT).unwrap();
        let all = backgrounds(conversation.events());
        let ended: Vec<(u64, Option<&str>)> = all
            .iter()
            .map(|one| (one.number, one.ended.as_deref()))
            .collect();
        assert_eq!(
            ended,
            [
                (1, Some(PROCESS_LOST)),
                (2, Some("killed")),
                (3, Some(PROCESS_LOST)),
                (2, Some(PROCESS_LOST))
            ]
        );
        let first = all.first().unwrap();
        assert_eq!(first.command, "make");
        assert_eq!(
            Some(first.started),
            conversation.events().first().map(|e| e.time)
        );
        drop(conversation);
        let (conversation, _) = Conversation::open(&state, &id, None, LOCK_WAIT).unwrap();
        assert_eq!(conversation.events().len(), 8, "recorded once");
    }

    /// An older td-agent's step snapshots and undo and redo records, as
    /// it wrote them, are read as retired, so its logs still open; any
    /// other kind unknown still refuses the log.
    #[test]
    fn an_older_logs_snapshot_and_undo_records_are_read_as_retired() {
        for (line, kind, trees) in [
            (
                r#"{"seq":5,"time":1,"kind":"snapshot","reply":3,"background":[1],"worktrees":[{"checkout":"/w/td","before":"aaaa","after":"bbbb","changed":["a"],"more":0}]}"#,
                "snapshot",
                vec![("/w/td".to_string(), "bbbb".to_string())],
            ),
            (
                r#"{"seq":6,"time":1,"kind":"undo","step":5}"#,
                "undo",
                Vec::new(),
            ),
            (
                r#"{"seq":7,"time":1,"kind":"redo","step":5}"#,
                "redo",
                Vec::new(),
            ),
        ] {
            let event = Event::from_json(&td_json::parse(line).unwrap()).unwrap();
            assert_eq!(
                event.kind,
                Kind::Retired {
                    kind: kind.into(),
                    trees
                }
            );
        }
        let unknown = td_json::parse(r#"{"seq":8,"time":1,"kind":"rewound"}"#).unwrap();
        assert!(Event::from_json(&unknown).is_err());
    }

    #[test]
    fn the_tool_events_replay_exactly_and_pausing_is_kept_in_meta() {
        let scratch = Scratch::new("tools");
        let state = scratch.state();
        let id = Id::random().unwrap();
        let from = Id::random().unwrap();
        let call = |n: &str| Call {
            id: format!("call_{n}"),
            name: "todo_write".into(),
            arguments: "{\"items\":[]}".into(),
        };
        let kinds = vec![
            Kind::Message {
                delivery: "d1".into(),
                from: from.clone(),
                role: Role::Orchestrator,
                text: "look \"here\"".into(),
                status: None,
                // One that started no turn: an unheld one would be given
                // its turn at load.
                held: Some(Held::Paused),
            },
            Kind::Message {
                delivery: "d2".into(),
                from,
                role: Role::Conversation,
                text: "done".into(),
                status: Some("done".into()),
                held: Some(Held::Budget),
            },
            Kind::Assistant {
                request: 1,
                content: None,
                reasoning: None,
                details: None,
                finish: "tool_calls".into(),
                incomplete: false,
                calls: vec![call("1"), call("2")],
            },
            Kind::ToolCall {
                reply: 3,
                id: "call_1".into(),
                name: "todo_write".into(),
            },
            Kind::ToolResult {
                reply: 3,
                id: "call_1".into(),
                name: "todo_write".into(),
                call: 4,
                content: "The todo list is empty.".into(),
                error: false,
                kept: None,
                digest: None,
                // A patch's, one file written and one gone.
                digests: vec![("/w/a".into(), Some("d1".into())), ("/w/b".into(), None)],
            },
            Kind::ToolResult {
                reply: 3,
                id: "call_2".into(),
                name: "todo_write".into(),
                call: 0,
                content: CALL_NOT_RUN.into(),
                error: true,
                kept: None,
                digest: None,
                digests: Vec::new(),
            },
            Kind::Todo {
                items: vec![TodoItem {
                    content: "x".into(),
                    status: Status::InProgress,
                }],
                cleared: false,
            },
            Kind::Pause { paused: true },
            Kind::Approval {
                call: 4,
                outcome: "allowed".into(),
                by: "a rule".into(),
                probabilities: None,
                reason: Some("read-only".into()),
            },
            Kind::Retired {
                kind: "undo".into(),
                trees: Vec::new(),
            },
            Kind::Retired {
                kind: "redo".into(),
                trees: Vec::new(),
            },
            Kind::Process {
                number: 1,
                call: 4,
                command: "make \"all\"\n".into(),
            },
            Kind::Ended {
                number: 1,
                how: "exit status 2".into(),
                tail: Some("built\n<U+001B>[0m".into()),
                held: Some(Held::Budget),
            },
            Kind::Retired {
                kind: "snapshot".into(),
                trees: vec![("/w/td".into(), "b".repeat(40))],
            },
        ];
        let written = {
            let (mut conversation, _) =
                Conversation::open(&state, &id, Some(Role::Conversation), LOCK_WAIT).unwrap();
            for kind in kinds {
                conversation.append(kind).unwrap();
            }
            conversation.set_paused(true).unwrap();
            conversation.sync().unwrap();
            assert!(conversation.delivered("d2"), "a message's delivery counts");
            conversation.events().to_vec()
        };
        let (conversation, load) = Conversation::open(&state, &id, None, LOCK_WAIT).unwrap();
        assert_eq!(load, Load::default(), "every call has its result");
        assert_eq!(conversation.events(), written.as_slice());
        assert!(conversation.meta().paused);
        assert_eq!(read_log(&state, &id).unwrap(), written);
        // A resumption logged by a process that died before writing
        // `meta`: the log wins at the next open.
        let mut conversation = conversation;
        conversation.append(Kind::Pause { paused: false }).unwrap();
        conversation.sync().unwrap();
        drop(conversation);
        let (conversation, _) = Conversation::open(&state, &id, None, LOCK_WAIT).unwrap();
        assert!(!conversation.meta().paused);
        drop(conversation);
        let (metas, _) = state.list();
        assert!(metas.iter().all(|m| !m.paused), "and `meta` is written");
    }

    #[test]
    fn archiving_is_kept_in_meta_under_the_conversations_lock() {
        let scratch = Scratch::new("archive");
        let state = scratch.state();
        let id = Id::random().unwrap();
        let (conversation, _) =
            Conversation::open(&state, &id, Some(Role::Conversation), LOCK_WAIT).unwrap();
        assert!(!conversation.meta().archived);
        // Its process holds the lock: the window waits, then says so.
        let refused = state
            .set_archived(&id, true, false, Duration::from_millis(50))
            .unwrap_err();
        assert!(refused.contains("lock"), "{refused}");
        drop(conversation);
        // Its background processes' output goes with archiving, and
        // unarchiving leaves another's be.
        let outputs = state.conversation(&id).join(crate::output::DIR);
        let mut writer = crate::output::Writer::create(&outputs, 1, 1024).unwrap();
        writer.push(b"built").unwrap();
        // An archive that is not stored removes nothing.
        let meta = std::fs::read(state.conversation(&id).join("meta")).unwrap();
        std::fs::write(state.conversation(&id).join("meta"), "torn").unwrap();
        assert!(state.set_archived(&id, true, false, LOCK_WAIT).is_err());
        assert!(outputs.is_dir());
        std::fs::write(state.conversation(&id).join("meta"), meta).unwrap();
        state.set_archived(&id, false, false, LOCK_WAIT).unwrap();
        assert!(outputs.is_dir());
        state.set_archived(&id, true, false, LOCK_WAIT).unwrap();
        assert!(!outputs.exists());
        let (metas, _) = state.list();
        assert!(metas.iter().all(|m| m.archived && m.id == id));
        // A process that opens it keeps it, and writes it back with what
        // it writes itself.
        let (mut conversation, _) = Conversation::open(&state, &id, None, LOCK_WAIT).unwrap();
        assert!(conversation.meta().archived);
        conversation.set_paused(true).unwrap();
        drop(conversation);
        assert!(state.list().0.iter().all(|m| m.archived && m.paused));
        state.set_archived(&id, false, false, LOCK_WAIT).unwrap();
        assert!(state.list().0.iter().all(|m| !m.archived && m.paused));
        assert_eq!(state.removed(&id), Ok(false));
        // A workspace gone with an archive stays gone once unarchived,
        // and a process's own writes keep it so.
        state.set_archived(&id, true, true, LOCK_WAIT).unwrap();
        state.set_archived(&id, false, false, LOCK_WAIT).unwrap();
        assert_eq!(state.removed(&id), Ok(true));
        let (mut conversation, _) = Conversation::open(&state, &id, None, LOCK_WAIT).unwrap();
        assert!(conversation.meta().removed);
        conversation.set_paused(false).unwrap();
        drop(conversation);
        assert!(state.list().0.iter().all(|m| m.removed && !m.paused));
        // A conversation the store does not hold is not made.
        let other = Id::random().unwrap();
        assert!(state.set_archived(&other, true, false, LOCK_WAIT).is_err());
        assert!(!state.conversation(&other).exists());
    }

    #[test]
    fn repository_rules_are_recorded_beside_the_instructions_within_their_bound() {
        use crate::repo::Instructions;
        use crate::rules::{Read, Rule, MAX_CARRIED};
        let scratch = Scratch::new("ruled");
        let state = scratch.state();
        let id = Id::random().unwrap();
        let ruled = |checkout: &str, base: char, rules: Read| Instructed {
            checkout: checkout.into(),
            base: base.to_string().repeat(40),
            read: Instructions::Absent,
            rules,
        };
        let rule = Rule::parse(&format!("deny shell {}", "x".repeat(500))).unwrap();
        let half = Read::Found(vec![rule; MAX_CARRIED / 2 / 512]);
        assert_eq!(half.carried(), MAX_CARRIED / 2);
        let (mut conversation, _) =
            Conversation::open(&state, &id, Some(Role::Conversation), LOCK_WAIT).unwrap();
        conversation
            .instructed(vec![
                ruled("/w/a", 'a', half.clone()),
                ruled("/w/b", 'b', half.clone()),
                // A commit's rules count once.
                ruled("/w/a2", 'a', half.clone()),
            ])
            .unwrap();
        // Another commit's would pass the bound, so they are recorded
        // unread, and every acting call asks.
        conversation
            .instructed(vec![ruled(
                "/w/c",
                'c',
                Read::Found(vec![Rule::parse("ask glob").unwrap()]),
            )])
            .unwrap();
        drop(conversation);
        let (conversation, _) = Conversation::open(&state, &id, None, LOCK_WAIT).unwrap();
        let rules: Vec<&Read> = conversation
            .instructions()
            .iter()
            .map(|one| &one.rules)
            .collect();
        assert_eq!(rules.get(..3).unwrap(), [&half, &half, &half]);
        assert!(
            matches!(rules.get(3).unwrap(), Read::Unread { why } if why.contains("past")),
            "{rules:?}"
        );
        // A record from before rules were read says nothing of them: not
        // read, so the table asks.
        let old = td_json::parse(&format!(
            r#"{{"checkout":"/w/a","base":"{}","read":{{"kind":"absent"}}}}"#,
            "a".repeat(40)
        ))
        .unwrap();
        assert!(matches!(
            Instructed::from_json(&old, &[], 0).unwrap().rules,
            Read::Unread { why } if why.contains("recorded before")
        ));
    }

    #[test]
    fn project_instructions_are_recorded_once_a_worktree_within_their_bound() {
        use crate::repo::Instructions;
        let scratch = Scratch::new("instructed");
        let state = scratch.state();
        let id = Id::random().unwrap();
        let instructed = |checkout: &str, read: Instructions| Instructed {
            checkout: checkout.into(),
            base: "a".repeat(40),
            read,
            rules: Default::default(),
        };
        let found = |text: &str| Instructions::Found {
            name: "AGENTS.md".into(),
            text: text.into(),
        };
        let (mut conversation, _) =
            Conversation::open(&state, &id, Some(Role::Conversation), LOCK_WAIT).unwrap();
        assert!(conversation.instructions().is_empty());
        conversation
            .instructed(vec![
                instructed("/w/a", found("first")),
                instructed("/w/b", Instructions::Absent),
            ])
            .unwrap();
        // Read again, as for a repository not yet prepared, a worktree's
        // record is replaced, in its place; a commit's text counts once.
        let big = found(&"x".repeat(MAX_INSTRUCTED - "second".len()));
        conversation
            .instructed(vec![
                instructed("/w/a", found("second")),
                instructed("/w/d", big.clone()),
                instructed("/w/e", big.clone()),
                instructed("/w/c", found("past the bound")),
            ])
            .unwrap();
        let kept = conversation.instructions().to_vec();
        assert_eq!(kept.len(), 5);
        assert_eq!(kept.first().map(|i| &i.read), Some(&found("second")));
        assert_eq!(kept.get(2).map(|i| &i.read), Some(&big));
        assert_eq!(kept.get(3).map(|i| &i.read), Some(&big));
        assert!(
            matches!(kept.get(4).map(|i| &i.read), Some(Instructions::Unread { why }) if why.contains("past")),
            "not said past the bound"
        );
        // Read again larger, one worktree's record is bounded against the
        // others, which stay as they were.
        conversation
            .instructed(vec![instructed("/w/a", found(&"y".repeat(MAX_INSTRUCTED)))])
            .unwrap();
        let again = conversation.instructions().to_vec();
        assert!(matches!(
            again.first().map(|i| &i.read),
            Some(Instructions::Unread { .. })
        ));
        assert_eq!(again.get(1..), kept.get(1..));
        let kept = again;
        drop(conversation);
        let (conversation, _) = Conversation::open(&state, &id, None, LOCK_WAIT).unwrap();
        assert_eq!(conversation.instructions(), kept.as_slice());
        drop(conversation);
        // What td-agent would not have written refuses the conversation:
        // a relative checkout, a base that is no commit, another file.
        let file = state.conversation(&id).join(INSTRUCTIONS);
        let commit = "a".repeat(40);
        for (checkout, base, read) in [
            ("rel", commit.as_str(), r#"{"kind": "absent"}"#),
            ("/w/a", "main", r#"{"kind": "absent"}"#),
            (
                "/w/a",
                commit.as_str(),
                r#"{"kind": "found", "name": "README.md", "text": "x"}"#,
            ),
        ] {
            std::fs::write(
                &file,
                format!(r#"[{{"checkout": "{checkout}", "base": "{base}", "read": {read}}}]"#),
            )
            .unwrap();
            assert!(
                Conversation::open(&state, &id, None, LOCK_WAIT).is_err(),
                "{checkout} {base} {read}"
            );
        }
    }

    /// A workspace's every worktree at one commit, its text the most the
    /// record holds and escaping most, is written within the file's bound
    /// and read back.
    #[test]
    fn worktrees_at_one_commit_keep_their_instructions_file_within_its_bound() {
        let scratch = Scratch::new("instructed-shared");
        let state = scratch.state();
        let id = Id::random().unwrap();
        let (mut conversation, _) =
            Conversation::open(&state, &id, Some(Role::Conversation), LOCK_WAIT).unwrap();
        let read = crate::repo::Instructions::Found {
            name: "AGENTS.md".into(),
            text: "\u{1}".repeat(MAX_INSTRUCTED),
        };
        // And rules at their bound, which count and are written once a
        // commit too.
        let rule = crate::rules::Rule::parse(&format!("deny shell {}", "x".repeat(500))).unwrap();
        let rules = crate::rules::Read::Found(vec![rule; crate::rules::MAX_CARRIED / 512]);
        let all: Vec<Instructed> = (0..crate::workspace::MAX_ENTRIES)
            .map(|n| Instructed {
                checkout: PathBuf::from(format!("/w/{n}")),
                base: "a".repeat(40),
                read: read.clone(),
                rules: rules.clone(),
            })
            .collect();
        conversation.instructed(all.clone()).unwrap();
        assert_eq!(conversation.instructions(), all.as_slice());
        drop(conversation);
        let bytes = std::fs::metadata(state.conversation(&id).join(INSTRUCTIONS))
            .unwrap()
            .len();
        assert!(bytes <= MAX_INSTRUCTIONS_FILE, "{bytes}");
        let (conversation, _) = Conversation::open(&state, &id, None, LOCK_WAIT).unwrap();
        assert_eq!(conversation.instructions(), all.as_slice());
        drop(conversation);
        // A reference to no earlier entry at its commit refuses the file.
        std::fs::write(
            state.conversation(&id).join(INSTRUCTIONS),
            format!(
                r#"[{{"checkout": "/w/0", "base": "{a}", "read": {{"kind": "absent"}}}}, {{"checkout": "/w/1", "base": "{b}", "same": 0}}]"#,
                a = "a".repeat(40),
                b = "b".repeat(40)
            ),
        )
        .unwrap();
        assert!(Conversation::open(&state, &id, None, LOCK_WAIT).is_err());
        // As does one for rules.
        std::fs::write(
            state.conversation(&id).join(INSTRUCTIONS),
            format!(
                r#"[{{"checkout": "/w/0", "base": "{a}", "read": {{"kind": "absent"}}, "rules": {{"kind": "absent"}}}}, {{"checkout": "/w/1", "base": "{b}", "read": {{"kind": "absent"}}, "same_rules": 0}}]"#,
                a = "a".repeat(40),
                b = "b".repeat(40)
            ),
        )
        .unwrap();
        assert!(Conversation::open(&state, &id, None, LOCK_WAIT).is_err());
    }

    #[test]
    fn the_humans_rules_are_read_whole_or_refused() {
        let scratch = Scratch::new("human-rules");
        let state = scratch.state();
        assert_eq!(state.load_rules().unwrap(), "");
        let path = state.root().join(crate::rules::HUMAN_FILE);
        let text = "[everywhere]\ndeny shell rm\n";
        std::fs::write(&path, text).unwrap();
        assert_eq!(state.load_rules().unwrap(), text);
        std::fs::write(&path, "[everywhere]\nallow shell ls\n").unwrap();
        let e = state.load_rules().unwrap_err();
        assert!(
            e.ends_with("line 2: an allow is for one workspace, not every one"),
            "{e}"
        );
        std::fs::write(&path, vec![b'#'; crate::rules::MAX_HUMAN_FILE + 1]).unwrap();
        assert!(state.load_rules().is_err());
        std::fs::write(&path, [0xff]).unwrap();
        assert!(state.load_rules().unwrap_err().ends_with("is not UTF-8"));
    }

    /// The templates made in the window are none until saved, read back as
    /// saved, and a file td-agent would not have written is refused and
    /// can be set aside.
    #[test]
    fn templates_made_in_the_window_are_saved_and_read_back() {
        let scratch = Scratch::new("templates");
        let state = scratch.state();
        assert!(state.load_templates().unwrap().is_empty());
        let templates = vec![crate::config::Template {
            network: None,
            name: "td".into(),
            repos: vec![crate::config::checked_repo("/srv/git/td", "main", "agent", None).unwrap()],
            shared: None,
        }];
        state.save_templates(&templates).unwrap();
        assert_eq!(state.load_templates().unwrap(), templates);
        state.save_templates(&[]).unwrap();
        assert!(state.load_templates().unwrap().is_empty());
        let unchecked = vec![crate::config::Template {
            network: None,
            name: "td".into(),
            repos: vec![crate::config::Repo {
                remote: "/srv/git/td".into(),
                base: "main".into(),
                branch: "agent".into(),
                sparse: None,
            }],
            shared: None,
        }];
        assert!(state.save_templates(&unchecked).is_err());
        std::fs::write(state.root().join(TEMPLATES), "[{\"name\":\"td\"}]").unwrap();
        assert!(state.load_templates().is_err());
        let aside = state.set_templates_aside().unwrap();
        assert!(aside.is_file());
        assert!(state.load_templates().unwrap().is_empty());
    }

    #[test]
    fn remotes_admitted_on_cards_are_kept_once_and_read_back() {
        let scratch = Scratch::new("admitted");
        let state = scratch.state();
        assert!(state.load_admitted().unwrap().is_empty());
        let url = |text| crate::git::Remote::parse(text).unwrap().url();
        let both = [url("https://example.org/a/td"), url("git@example.org:a/b")];
        let admitted = state
            .admit(&[
                "HTTPS://Example.org/a/td/".into(),
                "git@example.org:a/b".into(),
            ])
            .unwrap();
        assert_eq!(admitted, both);
        // Admitted again, kept once; read back as written.
        assert_eq!(state.admit(&[both[0].clone()]).unwrap(), both);
        assert_eq!(state.load_admitted().unwrap(), both);
        assert!(state.admit(&["http://example.org/a".into()]).is_err());
        assert_eq!(state.load_admitted().unwrap(), both);
        // A line td-agent would not have written refuses the file.
        for line in [
            "HTTPS://Example.org/a/td/",
            "git://example.org/x",
            "not a remote",
        ] {
            std::fs::write(state.root().join(ADMITTED), format!("{line}\n")).unwrap();
            assert!(state.load_admitted().is_err(), "{line}");
            assert!(state.admit(&both).is_err(), "{line}");
        }
        // Set aside, no remote in it is admitted, and a card admits again.
        let aside = state.set_admitted_aside().unwrap();
        assert!(aside.is_file());
        assert!(state.load_admitted().unwrap().is_empty());
        assert_eq!(state.admit(&both).unwrap(), both);
        // More lines than are admitted at most refuse the file.
        let many: String = (0..=MAX_ADMITTED)
            .map(|n| format!("https://example.org/r{n}\n"))
            .collect();
        std::fs::write(state.root().join(ADMITTED), many).unwrap();
        let e = state.load_admitted().unwrap_err();
        assert!(e.contains("more than 256"), "{e}");
    }

    #[test]
    fn prepared_repositories_are_kept_in_meta_once() {
        let scratch = Scratch::new("prepared");
        let state = scratch.state();
        let id = Id::random().unwrap();
        let (mut conversation, _) =
            Conversation::open(&state, &id, Some(Role::Conversation), LOCK_WAIT).unwrap();
        assert!(conversation.meta().prepared.is_empty());
        let repository = Path::new("/d/ws/td-1/td.git");
        conversation.set_prepared(repository).unwrap();
        conversation.set_prepared(repository).unwrap();
        drop(conversation);
        // Kept by the window's archiving, and read back whole.
        state.set_archived(&id, true, false, LOCK_WAIT).unwrap();
        let (conversation, _) = Conversation::open(&state, &id, None, LOCK_WAIT).unwrap();
        assert_eq!(conversation.meta().prepared, [repository.to_path_buf()]);
        assert!(conversation.meta().archived);
        // A meta written before has none; a relative path is refused.
        let mut value = conversation.meta().to_json();
        if let Json::Obj(pairs) = &mut value {
            pairs.retain(|(key, _)| key != "prepared");
        }
        assert!(Meta::from_json(&value).unwrap().prepared.is_empty());
        if let Json::Obj(pairs) = &mut value {
            pairs.push(("prepared".into(), Json::Arr(vec![Json::Str("rel".into())])));
        }
        assert!(Meta::from_json(&value).is_err());
        // Nor has it `tracked`, each entry a remote, a base and a commit;
        // a later record of a base replaces the earlier.
        let mut value = conversation.meta().to_json();
        if let Json::Obj(pairs) = &mut value {
            pairs.retain(|(key, _)| key != "tracked");
        }
        assert!(Meta::from_json(&value).unwrap().tracked.is_empty());
        if let Json::Obj(pairs) = &mut value {
            pairs.push((
                "tracked".into(),
                Json::Arr(vec![Json::Obj(vec![
                    ("remote".into(), Json::Str("r".into())),
                    ("base".into(), Json::Str("main".into())),
                    ("id".into(), Json::Str("main".into())),
                ])]),
            ));
        }
        assert!(Meta::from_json(&value).is_err(), "a commit that is no id");
        drop(conversation);
        let (mut conversation, _) = Conversation::open(&state, &id, None, LOCK_WAIT).unwrap();
        let (a, b) = ("a".repeat(40), "b".repeat(40));
        conversation
            .set_tracked(
                "r",
                &[("main".into(), a.clone()), ("next".into(), a.clone())],
            )
            .unwrap();
        conversation
            .set_tracked("r", &[("main".into(), b.clone())])
            .unwrap();
        conversation
            .set_tracked("s", &[("main".into(), a.clone())])
            .unwrap();
        drop(conversation);
        let (conversation, _) = Conversation::open(&state, &id, None, LOCK_WAIT).unwrap();
        let read: Vec<(&str, &str, &str)> = conversation
            .meta()
            .tracked
            .iter()
            .map(|t| (t.remote.as_str(), t.base.as_str(), t.id.as_str()))
            .collect();
        assert_eq!(
            read,
            [
                ("r", "next", a.as_str()),
                ("r", "main", b.as_str()),
                ("s", "main", a.as_str())
            ]
        );
        // Nor has it `removed`, which must be a boolean.
        let mut value = conversation.meta().to_json();
        if let Json::Obj(pairs) = &mut value {
            pairs.retain(|(key, _)| key != "removed");
        }
        assert!(!Meta::from_json(&value).unwrap().removed);
        if let Json::Obj(pairs) = &mut value {
            pairs.push(("removed".into(), Json::Str("yes".into())));
        }
        assert!(Meta::from_json(&value).is_err());
    }

    #[test]
    fn a_choice_of_model_and_effort_replays_and_is_kept_in_meta() {
        let scratch = Scratch::new("choice");
        let state = scratch.state();
        let id = Id::random().unwrap();
        let chosen = Kind::Choice {
            model: Some("openai/gpt-6".into()),
            effort: Some("high".into()),
        };
        let (mut conversation, _) =
            Conversation::open(&state, &id, Some(Role::Conversation), LOCK_WAIT).unwrap();
        assert_eq!(conversation.choice(), (None, None));
        assert_eq!(
            (
                conversation.meta().model.clone(),
                conversation.meta().effort.clone()
            ),
            (None, None)
        );
        conversation.append(chosen.clone()).unwrap();
        conversation
            .set_choice(Some("openai/gpt-6".into()), Some("high".into()))
            .unwrap();
        conversation.sync().unwrap();
        let written = conversation.events().to_vec();
        drop(conversation);
        let (conversation, _) = Conversation::open(&state, &id, None, LOCK_WAIT).unwrap();
        assert_eq!(conversation.events(), written.as_slice());
        assert_eq!(written.last().map(|e| &e.kind), Some(&chosen));
        let both = (Some("openai/gpt-6".to_string()), Some("high".to_string()));
        assert_eq!(conversation.choice(), both);
        assert_eq!(
            (
                conversation.meta().model.clone(),
                conversation.meta().effort.clone()
            ),
            both
        );
        // Back to the configuration's, logged by a process that died
        // before writing `meta`: the log wins at the next open.
        let mut conversation = conversation;
        conversation
            .append(Kind::Choice {
                model: None,
                effort: Some("low".into()),
            })
            .unwrap();
        conversation.sync().unwrap();
        drop(conversation);
        let (conversation, _) = Conversation::open(&state, &id, None, LOCK_WAIT).unwrap();
        assert_eq!(conversation.choice(), (None, Some("low".into())));
        drop(conversation);
        let (metas, _) = state.list();
        assert_eq!(metas.len(), 1);
        assert_eq!(
            metas
                .iter()
                .map(|m| (m.model.clone(), m.effort.clone()))
                .next(),
            Some((None, Some("low".into()))),
            "and `meta` is written"
        );
    }

    #[test]
    fn a_whole_replys_calls_without_results_are_answered_at_load() {
        let scratch = Scratch::new("unanswered");
        let state = scratch.state();
        let id = Id::random().unwrap();
        let call = |n: &str| Call {
            id: n.into(),
            name: "history_search".into(),
            arguments: "{}".into(),
        };
        let reply = |calls: Vec<Call>, incomplete: bool| Kind::Assistant {
            request: 0,
            content: None,
            reasoning: None,
            details: None,
            finish: "tool_calls".into(),
            incomplete,
            calls,
        };
        {
            let (mut conversation, _) =
                Conversation::open(&state, &id, Some(Role::Conversation), LOCK_WAIT).unwrap();
            // An incomplete reply's calls never run and need no result.
            conversation.append(reply(vec![call("x")], true)).unwrap();
            conversation
                .append(reply(vec![call("a"), call("b"), call("c")], false))
                .unwrap();
            conversation
                .append(Kind::ToolCall {
                    reply: 2,
                    id: "a".into(),
                    name: "history_search".into(),
                })
                .unwrap();
            conversation
                .append(Kind::ToolResult {
                    reply: 2,
                    id: "a".into(),
                    name: "history_search".into(),
                    call: 3,
                    content: "No event matches.".into(),
                    error: false,
                    kept: None,
                    digest: None,
                    digests: Vec::new(),
                })
                .unwrap();
            conversation
                .append(Kind::ToolCall {
                    reply: 2,
                    id: "b".into(),
                    name: "history_search".into(),
                })
                .unwrap();
            conversation.sync().unwrap();
        }
        let (conversation, load) = Conversation::open(&state, &id, None, LOCK_WAIT).unwrap();
        assert_eq!(load.interrupted, [5]);
        let added: Vec<(&str, u64, &str)> = conversation
            .events()
            .get(5..)
            .unwrap()
            .iter()
            .filter_map(|e| match &e.kind {
                Kind::ToolResult {
                    id, call, content, ..
                } => Some((id.as_str(), *call, content.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(added, [("b", 5, CALL_INTERRUPTED), ("c", 0, CALL_NOT_RUN)]);
        drop(conversation);
        let (conversation, load) = Conversation::open(&state, &id, None, LOCK_WAIT).unwrap();
        assert_eq!(load, Load::default(), "answered once");
        assert_eq!(conversation.events().len(), 7);
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
        // Waiting, as a writer does: a child another test forked while
        // the lock was held holds it too until it execs.
        Conversation::open(&state, &id, None, LOCK_WAIT).unwrap();
    }

    #[test]
    fn a_second_window_lock_is_refused_while_the_first_is_held() {
        let scratch = Scratch::new("window");
        let state = scratch.state();
        let held = state.lock_window().unwrap();
        let refused = state.lock_window().unwrap_err();
        assert!(refused.contains("another td-agent window"), "{refused}");
        drop(held);
        // Tried again for a while, as above: a child forked while it was
        // held holds it too until it execs.
        let deadline = Instant::now() + LOCK_WAIT;
        let mut again = state.lock_window();
        while again.is_err() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            again = state.lock_window();
        }
        again.unwrap();
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
    fn the_default_model_is_saved_whole_and_read_back() {
        let scratch = Scratch::new("default");
        let state = scratch.state();
        assert_eq!(state.load_default_model(), None);
        state.save_default_model("a/b", None).unwrap();
        assert_eq!(state.load_default_model(), Some(("a/b".into(), None)));
        // The longest ids round-trip.
        let longest = "m".repeat(crate::config::MAX_NAME);
        state.save_default_model(&longest, Some(&longest)).unwrap();
        assert_eq!(
            state.load_default_model(),
            Some((longest.clone(), Some(longest)))
        );
        state.save_default_model("a/b", Some("c/d")).unwrap();
        assert_eq!(
            state.load_default_model(),
            Some(("a/b".into(), Some("c/d".into())))
        );
        // Not a model id: refused, and the file kept.
        assert!(state.save_default_model("a b", None).is_err());
        assert!(state.save_default_model("a/b", Some("")).is_err());
        assert_eq!(state.load_default_model().unwrap().0, "a/b");
        for text in [
            "model a/b\n",
            "model a b\nover c/d\n",
            "over c/d\nmodel a/b\n",
            "model a/b\noverc/d\n",
            "model a/b\nover \n",
        ] {
            std::fs::write(state.root().join(DEFAULT_MODEL), text).unwrap();
            assert_eq!(state.load_default_model(), None, "{text:?}");
        }
        state.forget_default_model().unwrap();
        assert!(!state.root().join(DEFAULT_MODEL).exists());
        state.forget_default_model().unwrap();
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
